//! Decorated 5-tab Settings window (`Cài đặt`) with isolated draft state,
//! live validation, `Huỷ` / `Lưu thay đổi`, engine selector, and voice preview.

use super::overlay::dim_color;
use crate::config::{AppConfig, CaptureRegion, HotkeyConfig};
use crate::ocr::{resolve_wechat_paths_from_config, WeChatOcrPaths};
use crate::translate::{TranslationEngine, TranslatorStatus};
use crate::tts::{
    default_voices_dir, find_piper_voice, voice_files_present, PiperDownloadEvent, TtsEngineKind,
    AVAILABLE_VOICES, DEFAULT_PIPER_VOICE_ID, PIPER_VOICES,
};
use crate::ui::fonts::parse_hex_color;
use crate::ui::icons::{IconButton, UiIcon};
use eframe::egui;
use std::path::PathBuf;

/// Default outer logical dimensions for the `Cài đặt` window.
pub const SETTINGS_WINDOW_SIZE: [f32; 2] = [680.0, 480.0];

/// Sample Vietnamese sentence played when clicking `Nghe thử`.
pub const SETTINGS_PREVIEW_SAMPLE_TEXT: &str = "Xin chào, đây là giọng đọc thử của cap2voice.";

/// Built-in Edge-TTS rate options (`rate`, Vietnamese UI label).
pub const SPEECH_RATE_OPTIONS: &[(&str, &str)] = &[
    ("-20%", "-20% (Chậm)"),
    ("-10%", "-10%"),
    ("+0%", "0% (Mặc định)"),
    ("+10%", "+10%"),
    ("+20%", "+20% (Nhanh)"),
];

/// Capture FPS selector options (`fps`, Vietnamese UI label).
pub const CAPTURE_FPS_OPTIONS: &[(u32, &str)] = &[
    (5, "5 khung/giây"),
    (10, "10 khung/giây"),
    (15, "15 khung/giây"),
];

const CHINESE_COLOR_PRESETS: &[(&str, &str)] = &[
    ("#FFFFFF", "Trắng"),
    ("#FEF08A", "Vàng nhạt"),
    ("#E2E8F0", "Xám sáng"),
];

const VIETNAMESE_COLOR_PRESETS: &[(&str, &str)] = &[
    ("#4ADE80", "Xanh lá"),
    ("#38BDF8", "Xanh dương"),
    ("#FBBF24", "Vàng cam"),
];

/// Viewport ID used by `egui::Context::show_viewport_immediate` for `Cài đặt`.
pub fn settings_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("cap2voice_settings_window")
}

/// Five sidebar navigation tabs matching `AGENTS.md` and Mockup 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    General,
    Translation,
    Voice,
    Display,
    Hotkeys,
}

impl SettingsTab {
    /// Ordered list of all 5 sidebar sections.
    pub const ALL: [Self; 5] = [
        Self::General,
        Self::Translation,
        Self::Voice,
        Self::Display,
        Self::Hotkeys,
    ];

    /// Exact Vietnamese sidebar label (`docs/spec.md` §3.6).
    pub fn label(self) -> &'static str {
        match self {
            Self::General => "Chung",
            Self::Translation => "Dịch",
            Self::Voice => "Giọng đọc",
            Self::Display => "Hiển thị",
            Self::Hotkeys => "Phím tắt",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Self::General => "Tốc độ chụp, vùng phụ đề, WeChat OCR và hành vi cửa sổ",
            Self::Translation => "Engine dịch và độ nhạy phát hiện phụ đề",
            Self::Voice => "Engine giọng đọc (Edge-TTS / Piper), giọng, và nghe thử",
            Self::Display => "Cỡ chữ, độ mờ và màu sắc phụ đề trên overlay",
            Self::Hotkeys => "Phím tắt toàn cục khi xem video ở cửa sổ khác",
        }
    }
}

/// Async feedback banner for `Nghe thử`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AsyncFeedbackState {
    Idle,
    InProgress(String),
    Success(String),
    Error(String),
}

/// User actions emitted by `render_settings_ui` back to `Cap2VoiceApp`.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsCommand {
    Cancel,
    Save(Box<AppConfig>),
    SelectRegion,
    PreviewVoice {
        engine: TtsEngineKind,
        voice: String,
        rate: String,
        volume: f32,
    },
    /// Download a Piper voice on demand (Settings → Giọng đọc, "Tải giọng").
    /// The app spawns the background download and reports progress into
    /// the draft's `piper_download` state.
    DownloadPiperVoice {
        voice_id: String,
    },
    /// Clear the translation + TTS caches (A3 / gh #2): removes all cached
    /// text and audio from memory and disk.
    ClearCache,
}

/// Progress of an in-flight (or just finished) Piper voice download,
/// shown on the Voice tab. Plain data — the channel receiver lives on
/// `Cap2VoiceApp`, which drains it and updates this struct each frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiperDownloadProgress {
    pub voice_id: String,
    pub downloaded: u64,
    pub total: Option<u64>,
    /// User-facing status line ("Đang tải…", "Đã tải xong", "Lỗi: …").
    pub status: String,
    pub done: bool,
}

/// Isolated draft model backing the Settings window until saved or discarded.
#[derive(Debug, Clone)]
pub struct SettingsDraftState {
    pub active_tab: SettingsTab,
    original: AppConfig,
    pub draft: AppConfig,
    pub wxocr_path_input: String,
    pub weixin_dir_input: String,
    pub log_dir_input: String,
    ocr_paths_status: Result<WeChatOcrPaths, String>,
    pub validation_error: Option<String>,
    /// Snapshot of the live translator status taken when the Settings
    /// window opens (D1+A5); shown on the Dịch tab. `None` when no
    /// translator was available at open time.
    pub translator_status: Option<TranslatorStatus>,
    pub tts_preview_status: AsyncFeedbackState,
    /// Resolved Piper voices directory (`%APPDATA%\cap2voice\voices\piper`),
    /// `None` when it cannot be resolved (no APPDATA) — the download UI
    /// then shows a notice instead of the download button.
    pub piper_voices_dir: Option<PathBuf>,
    /// Progress of the Piper voice download started from this window.
    pub piper_download: Option<PiperDownloadProgress>,
}

fn optional_path_from_input(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

fn validate_single_hotkey(label: &str, raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("Phím tắt '{label}' không được để trống"));
    }
    Ok(trimmed.to_string())
}

fn validate_hotkeys(raw: &HotkeyConfig) -> Result<HotkeyConfig, String> {
    let toggle_start_stop = validate_single_hotkey("Bắt đầu / Dừng dịch", &raw.toggle_start_stop)?;
    let select_region = validate_single_hotkey("Chọn vùng phụ đề", &raw.select_region)?;
    let toggle_overlay = validate_single_hotkey("Hiện / Ẩn overlay", &raw.toggle_overlay)?;
    let toggle_mute = validate_single_hotkey("Tắt / Bật tiếng", &raw.toggle_mute)?;

    let items = [
        ("Bắt đầu / Dừng dịch", &toggle_start_stop),
        ("Chọn vùng phụ đề", &select_region),
        ("Hiện / Ẩn overlay", &toggle_overlay),
        ("Tắt / Bật tiếng", &toggle_mute),
    ];
    for (idx_a, (label_a, key_a)) in items.iter().enumerate() {
        for (label_b, key_b) in items.iter().skip(idx_a + 1) {
            if key_a.eq_ignore_ascii_case(key_b) {
                return Err(format!(
                    "Phím tắt '{label_a}' và '{label_b}' đang trùng nhau ({key_a})"
                ));
            }
        }
    }

    Ok(HotkeyConfig {
        toggle_start_stop,
        select_region,
        toggle_overlay,
        toggle_mute,
    })
}

fn normalize_hex_input(label: &str, raw: &str) -> Result<String, String> {
    let normalized = raw.trim().to_ascii_uppercase();
    if parse_hex_color(&normalized).is_none() {
        return Err(format!(
            "Màu chữ {label} phải đúng định dạng #RRGGBB (nhận '{}')",
            raw.trim()
        ));
    }
    Ok(normalized)
}

impl SettingsDraftState {
    /// Create an isolated draft initialized from the currently active `AppConfig`.
    pub fn from_config(config: &AppConfig) -> Self {
        let ocr_paths_status =
            resolve_wechat_paths_from_config(&config.general).map_err(|err| err.to_string());
        Self {
            active_tab: SettingsTab::General,
            original: config.clone(),
            draft: config.clone(),
            wxocr_path_input: config
                .general
                .wxocr_dll_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            weixin_dir_input: config
                .general
                .weixin_dir_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            log_dir_input: config
                .general
                .subtitle_log_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            ocr_paths_status,
            validation_error: None,
            translator_status: None,
            tts_preview_status: AsyncFeedbackState::Idle,
            piper_voices_dir: default_voices_dir().ok(),
            piper_download: None,
        }
    }

    /// Default voice id for a TTS engine (Edge → NamMinh, Piper → Ngọc Huyền).
    pub fn default_voice_for_engine(engine: TtsEngineKind) -> &'static str {
        match engine {
            TtsEngineKind::Edge => "vi-VN-NamMinhNeural",
            TtsEngineKind::Piper => DEFAULT_PIPER_VOICE_ID,
        }
    }

    /// Mark a Piper voice download as started (called when the user clicks
    /// "Tải giọng"); the app updates `piper_download` as progress arrives.
    pub fn start_piper_download(&mut self, voice_id: String) {
        self.piper_download = Some(PiperDownloadProgress {
            voice_id,
            downloaded: 0,
            total: None,
            status: "Đang bắt đầu tải…".to_string(),
            done: false,
        });
    }

    /// Apply one download event to the draft's progress state.
    pub fn apply_piper_download_event(&mut self, event: PiperDownloadEvent) {
        match event {
            PiperDownloadEvent::Progress { downloaded, total } => {
                if let Some(progress) = &mut self.piper_download {
                    progress.downloaded = downloaded;
                    progress.total = total;
                    let total_mb = total.map(|t| t as f64 / 1_048_576.0);
                    progress.status = match total_mb {
                        Some(total) => format!(
                            "Đang tải… {:.1} / {:.1} MB",
                            downloaded as f64 / 1_048_576.0,
                            total
                        ),
                        None => format!("Đang tải… {:.1} MB", downloaded as f64 / 1_048_576.0),
                    };
                }
            }
            PiperDownloadEvent::Done { voice_id } => {
                self.piper_download = Some(PiperDownloadProgress {
                    voice_id,
                    downloaded: 0,
                    total: None,
                    status: "Đã tải xong — sẵn sàng dùng".to_string(),
                    done: true,
                });
            }
            PiperDownloadEvent::Error(detail) => {
                if let Some(progress) = &mut self.piper_download {
                    progress.status = format!("Lỗi tải giọng: {detail}");
                    progress.done = false;
                }
            }
        }
    }

    /// Return the immutable baseline configuration snapshot opened with Settings.
    pub fn original(&self) -> &AppConfig {
        &self.original
    }

    fn build_synced_candidate(&self) -> AppConfig {
        let mut candidate = self.draft.clone();
        candidate.general.wxocr_dll_path = optional_path_from_input(&self.wxocr_path_input);
        candidate.general.weixin_dir_path = optional_path_from_input(&self.weixin_dir_input);
        candidate.general.subtitle_log_dir = optional_path_from_input(&self.log_dir_input);
        candidate
    }

    /// Return `true` when any editable setting differs from the opened baseline.
    pub fn is_dirty(&self) -> bool {
        self.build_synced_candidate() != self.original
    }

    /// Update only the draft capture region without altering `original` or persisting disk state.
    pub fn set_draft_region(&mut self, region: CaptureRegion) {
        self.draft.general.region = Some(region);
        self.validation_error = None;
    }

    /// Reset all 4 global hotkeys in the draft back to spec defaults (`Ctrl+Alt+S/R/H/M`).
    pub fn reset_hotkeys_to_default(&mut self) {
        self.draft.hotkeys = HotkeyConfig::default();
        self.validation_error = None;
    }

    /// Re-probe `wxocr.dll` and Weixin 4.x folder using current draft path inputs.
    pub fn refresh_ocr_status(&mut self) {
        self.draft.general.wxocr_dll_path = optional_path_from_input(&self.wxocr_path_input);
        self.draft.general.weixin_dir_path = optional_path_from_input(&self.weixin_dir_input);
        self.ocr_paths_status =
            resolve_wechat_paths_from_config(&self.draft.general).map_err(|err| err.to_string());
    }

    /// Parse string inputs, validate all fields + hotkeys, and return committed `AppConfig`.
    pub fn try_commit(&mut self) -> Result<AppConfig, String> {
        let mut candidate = self.build_synced_candidate();

        match normalize_hex_input("tiếng Trung", &candidate.display.chinese_color_hex) {
            Ok(hex) => candidate.display.chinese_color_hex = hex,
            Err(err) => {
                self.validation_error = Some(err.clone());
                return Err(err);
            }
        }
        match normalize_hex_input("tiếng Việt", &candidate.display.vietnamese_color_hex) {
            Ok(hex) => candidate.display.vietnamese_color_hex = hex,
            Err(err) => {
                self.validation_error = Some(err.clone());
                return Err(err);
            }
        }

        match validate_hotkeys(&candidate.hotkeys) {
            Ok(hotkeys) => candidate.hotkeys = hotkeys,
            Err(err) => {
                self.validation_error = Some(err.clone());
                return Err(err);
            }
        }

        if let Err(err) = candidate.validate() {
            let msg = format!("Cấu hình không hợp lệ: {err}");
            self.validation_error = Some(msg.clone());
            return Err(msg);
        }

        self.validation_error = None;
        self.draft = candidate.clone();
        Ok(candidate)
    }

    /// Discard unsaved draft edits and return a clone of the original `AppConfig`.
    pub fn discard(&self) -> AppConfig {
        self.original.clone()
    }
}

/// Build the decorated, opaque dark `ViewportBuilder` for the `Cài đặt` window.
pub fn build_settings_viewport_builder() -> egui::ViewportBuilder {
    egui::ViewportBuilder::default()
        .with_title("Cài đặt")
        .with_inner_size(SETTINGS_WINDOW_SIZE)
        .with_min_inner_size([600.0, 420.0])
        .with_decorations(true)
        .with_transparent(false)
        .with_resizable(true)
        .with_active(true)
}

fn muted_text_color() -> egui::Color32 {
    egui::Color32::from_rgb(148, 163, 184)
}

fn accent_blue_color() -> egui::Color32 {
    egui::Color32::from_rgb(37, 99, 235)
}

fn format_region_summary(region: Option<&CaptureRegion>) -> String {
    match region {
        Some(r) => format!(
            "{} · {}, {} · {} × {} px",
            r.monitor_id, r.x, r.y, r.width, r.height
        ),
        None => "Chưa chọn vùng".to_string(),
    }
}

fn render_async_feedback(ui: &mut egui::Ui, status: &AsyncFeedbackState) {
    match status {
        AsyncFeedbackState::Idle => {}
        AsyncFeedbackState::InProgress(msg) => {
            ui.label(
                egui::RichText::new(msg)
                    .size(12.5)
                    .color(egui::Color32::from_rgb(250, 204, 21)),
            );
        }
        AsyncFeedbackState::Success(msg) => {
            ui.label(
                egui::RichText::new(msg)
                    .size(12.5)
                    .color(egui::Color32::from_rgb(74, 222, 128)),
            );
        }
        AsyncFeedbackState::Error(msg) => {
            ui.label(
                egui::RichText::new(msg)
                    .size(12.5)
                    .color(egui::Color32::from_rgb(248, 113, 113)),
            );
        }
    }
}

fn render_general_tab(
    ui: &mut egui::Ui,
    state: &mut SettingsDraftState,
    emitted: &mut Option<SettingsCommand>,
) {
    egui::Grid::new("settings_general_grid")
        .num_columns(2)
        .spacing([18.0, 12.0])
        .show(ui, |ui| {
            ui.label("Tốc độ chụp");
            let fps_label = CAPTURE_FPS_OPTIONS
                .iter()
                .find(|(fps, _)| *fps == state.draft.general.capture_fps)
                .map(|(_, label)| *label)
                .unwrap_or("Tuỳ chỉnh");
            egui::ComboBox::from_id_salt("settings_capture_fps")
                .selected_text(fps_label)
                .width(190.0)
                .show_ui(ui, |ui| {
                    for (fps, label) in CAPTURE_FPS_OPTIONS {
                        ui.selectable_value(&mut state.draft.general.capture_fps, *fps, *label);
                    }
                });
            ui.end_row();

            ui.label("Vùng phụ đề");
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format_region_summary(state.draft.general.region.as_ref()))
                        .size(12.5)
                        .color(muted_text_color()),
                );
                if ui
                    .add(IconButton::small(UiIcon::Region))
                    .on_hover_text("Chọn lại vùng")
                    .clicked()
                {
                    *emitted = Some(SettingsCommand::SelectRegion);
                }
            });
            ui.end_row();

            ui.label("Đường dẫn wxocr.dll");
            ui.add(
                egui::TextEdit::singleline(&mut state.wxocr_path_input)
                    .desired_width(280.0)
                    .hint_text("Tự động nhận diện (để trống để tự tìm)"),
            );
            ui.end_row();

            ui.label("Thư mục Weixin 4.x");
            ui.add(
                egui::TextEdit::singleline(&mut state.weixin_dir_input)
                    .desired_width(280.0)
                    .hint_text("Tự động nhận diện (để trống để tự tìm)"),
            );
            ui.end_row();
        });

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui
            .add(IconButton::small(UiIcon::Check))
            .on_hover_text("Kiểm tra WeChat OCR")
            .clicked()
        {
            state.refresh_ocr_status();
        }
        match &state.ocr_paths_status {
            Ok(paths) => {
                ui.label(
                    egui::RichText::new(format!("OK: {}", paths.wxocr_dll.display()))
                        .size(12.0)
                        .color(egui::Color32::from_rgb(74, 222, 128)),
                );
            }
            Err(err) => {
                ui.label(
                    egui::RichText::new(err)
                        .size(12.0)
                        .color(egui::Color32::from_rgb(248, 113, 113)),
                );
            }
        }
    });

    ui.add_space(10.0);
    ui.separator();
    ui.add_space(8.0);

    ui.checkbox(
        &mut state.draft.general.enable_subtitle_log,
        "Ghi nhật ký cặp phụ đề (Chinese : Vietnamese)",
    );
    ui.horizontal(|ui| {
        ui.label("Thư mục log");
        ui.add_enabled(
            state.draft.general.enable_subtitle_log,
            egui::TextEdit::singleline(&mut state.log_dir_input)
                .desired_width(290.0)
                .hint_text(r"Mặc định (%APPDATA%\cap2voice\logs)"),
        );
    });

    ui.add_space(8.0);
    ui.checkbox(
        &mut state.draft.general.debug_logging,
        "Ghi log debug OCR (chẩn đoán câu miss / câu gộp)",
    );
    ui.label(
        egui::RichText::new(
            "Khi bật: ghi ocr-debug-NGÀY.log vào thư mục log — raw OCR từng frame kèm quyết định pixel-diff / dedupe / hold / buffer. Áp dụng khi bắt đầu dịch lần sau. Mặc định tắt.",
        )
        .size(12.0)
        .color(muted_text_color()),
    );

    ui.add_space(8.0);
    ui.checkbox(
        &mut state.draft.general.close_to_tray,
        "Ẩn xuống khay hệ thống khi đóng cửa sổ",
    );
    ui.checkbox(
        &mut state.draft.general.start_with_windows,
        "Khởi động cùng Windows",
    );
    ui.checkbox(
        &mut state.draft.general.start_minimized_to_tray,
        "Khởi động ở chế độ ẩn xuống khay",
    );
}

/// One-line live translator status for the Dịch tab, plus its text color.
/// `None` means no translator was available when Settings opened.
fn translator_status_text(status: &Option<TranslatorStatus>) -> (String, egui::Color32) {
    use crate::translate::HelsinkiState;
    let green = egui::Color32::from_rgb(74, 222, 128);
    let yellow = egui::Color32::from_rgb(250, 204, 21);
    let red = egui::Color32::from_rgb(248, 113, 113);
    match status {
        None => (
            "Không có engine dịch khả dụng — kiểm tra model/từ điển".to_string(),
            red,
        ),
        Some(status) => {
            let helsinki = match &status.helsinki {
                HelsinkiState::NotLoaded => "Helsinki: chưa tải".to_string(),
                HelsinkiState::Loading => "Helsinki: đang tải model…".to_string(),
                HelsinkiState::Ready => "Helsinki: sẵn sàng".to_string(),
                HelsinkiState::Failed(detail) => format!("Helsinki: lỗi ({detail})"),
            };
            let vietphrase = if status.vietphrase_available {
                "Vietphrase: sẵn sàng"
            } else {
                "Vietphrase: không khả dụng"
            };
            let fallbacks = if status.helsinki_fallbacks > 0 {
                format!(
                    " · đã dùng Vietphrase thay {} câu",
                    status.helsinki_fallbacks
                )
            } else {
                String::new()
            };
            let color = match (&status.engine, &status.helsinki) {
                (TranslationEngine::Helsinki, HelsinkiState::Failed(_))
                    if !status.vietphrase_available =>
                {
                    red
                }
                (TranslationEngine::Helsinki, HelsinkiState::Loading)
                | (TranslationEngine::Helsinki, HelsinkiState::NotLoaded) => yellow,
                _ => green,
            };
            (
                format!(
                    "Đang dùng: {} · {helsinki} · {vietphrase}{fallbacks} (lúc mở Cài đặt)",
                    status.engine.label()
                ),
                color,
            )
        }
    }
}

fn render_translation_tab(
    ui: &mut egui::Ui,
    state: &mut SettingsDraftState,
    emitted: &mut Option<SettingsCommand>,
) {
    egui::Grid::new("settings_translation_grid")
        .num_columns(2)
        .spacing([18.0, 12.0])
        .show(ui, |ui| {
            ui.label("Bộ dịch");
            egui::ComboBox::from_id_salt("settings_translation_engine")
                .selected_text(state.draft.translation.engine.label())
                .width(230.0)
                .show_ui(ui, |ui| {
                    for engine in TranslationEngine::ALL {
                        ui.selectable_value(
                            &mut state.draft.translation.engine,
                            engine,
                            engine.label(),
                        );
                    }
                });
            ui.end_row();

            ui.label("Trạng thái engine");
            let (status_text, status_color) = translator_status_text(&state.translator_status);
            ui.label(
                egui::RichText::new(status_text)
                    .size(12.5)
                    .color(status_color),
            );
            ui.end_row();

            ui.label("Ngưỡng trùng lặp phụ đề");
            ui.add(
                egui::Slider::new(&mut state.draft.translation.dedupe_threshold, 0.80..=0.99)
                    .fixed_decimals(2),
            );
            ui.end_row();

            ui.label("Độ nhạy khác biệt khung hình");
            let mut diff_pct = state.draft.translation.pixel_diff_threshold * 100.0;
            if ui
                .add(
                    egui::Slider::new(&mut diff_pct, 0.5..=10.0)
                        .fixed_decimals(1)
                        .suffix("%"),
                )
                .changed()
            {
                state.draft.translation.pixel_diff_threshold =
                    (diff_pct / 100.0).clamp(0.001, 0.50);
            }
            ui.end_row();

            ui.label("Số khung trống để reset trùng lặp");
            ui.add(egui::Slider::new(
                &mut state.draft.translation.blank_reset_frames,
                1..=30,
            ));
            ui.end_row();

            ui.label("Bộ nhớ đệm dịch + giọng đọc");
            ui.horizontal(|ui| {
                if ui
                    .add(IconButton::small(UiIcon::Trash))
                    .on_hover_text("Xóa bộ nhớ đệm")
                    .clicked()
                {
                    *emitted = Some(SettingsCommand::ClearCache);
                }
                ui.label(
                    egui::RichText::new("Xóa toàn bộ bản dịch và audio đã lưu (RAM + đĩa).")
                        .color(muted_text_color()),
                );
            });
            ui.end_row();
        });
}

fn render_voice_tab(
    ui: &mut egui::Ui,
    state: &mut SettingsDraftState,
    emitted: &mut Option<SettingsCommand>,
) {
    // Switching the engine resets the voice to that engine's default, so
    // the draft never pairs an Edge voice id with Piper (or vice versa).
    let prev_engine = state.draft.voice.engine;
    egui::Grid::new("settings_voice_grid")
        .num_columns(2)
        .spacing([18.0, 12.0])
        .show(ui, |ui| {
            ui.label("Engine giọng đọc");
            egui::ComboBox::from_id_salt("settings_tts_engine")
                .selected_text(state.draft.voice.engine.label())
                .width(230.0)
                .show_ui(ui, |ui| {
                    for engine in TtsEngineKind::ALL {
                        ui.selectable_value(&mut state.draft.voice.engine, engine, engine.label());
                    }
                });
            ui.end_row();

            ui.label("Giọng đọc");
            match state.draft.voice.engine {
                TtsEngineKind::Edge => {
                    let voice_label = AVAILABLE_VOICES
                        .iter()
                        .find(|(id, _)| *id == state.draft.voice.voice)
                        .map(|(_, label)| *label)
                        .unwrap_or(state.draft.voice.voice.as_str());
                    egui::ComboBox::from_id_salt("settings_voice_select")
                        .selected_text(voice_label)
                        .width(230.0)
                        .show_ui(ui, |ui| {
                            for (voice_id, label) in AVAILABLE_VOICES {
                                ui.selectable_value(
                                    &mut state.draft.voice.voice,
                                    (*voice_id).to_string(),
                                    *label,
                                );
                            }
                        });
                }
                TtsEngineKind::Piper => {
                    let voice_label = PIPER_VOICES
                        .iter()
                        .find(|voice| voice.id == state.draft.voice.voice)
                        .map(|voice| voice.label)
                        .unwrap_or(state.draft.voice.voice.as_str());
                    egui::ComboBox::from_id_salt("settings_piper_voice_select")
                        .selected_text(voice_label)
                        .width(230.0)
                        .show_ui(ui, |ui| {
                            for voice in PIPER_VOICES {
                                ui.selectable_value(
                                    &mut state.draft.voice.voice,
                                    voice.id.to_string(),
                                    voice.label,
                                );
                            }
                        });
                }
            }
            ui.end_row();

            ui.label("Âm lượng");
            let mut vol_pct = (state.draft.voice.volume * 100.0).round();
            if ui
                .add(
                    egui::Slider::new(&mut vol_pct, 0.0..=100.0)
                        .fixed_decimals(0)
                        .suffix("%"),
                )
                .changed()
            {
                state.draft.voice.volume = (vol_pct / 100.0).clamp(0.0, 1.0);
            }
            ui.end_row();

            ui.label("Tốc độ đọc");
            let rate_label = SPEECH_RATE_OPTIONS
                .iter()
                .find(|(rate, _)| *rate == state.draft.voice.speech_rate)
                .map(|(_, label)| *label)
                .unwrap_or(state.draft.voice.speech_rate.as_str());
            egui::ComboBox::from_id_salt("settings_speech_rate")
                .selected_text(rate_label)
                .width(190.0)
                .show_ui(ui, |ui| {
                    for (rate, label) in SPEECH_RATE_OPTIONS {
                        ui.selectable_value(
                            &mut state.draft.voice.speech_rate,
                            (*rate).to_string(),
                            *label,
                        );
                    }
                });
            ui.end_row();
        });
    if state.draft.voice.engine != prev_engine {
        state.draft.voice.voice =
            SettingsDraftState::default_voice_for_engine(state.draft.voice.engine).to_string();
        // A fresh engine has no download in flight for the new voice.
        state.piper_download = None;
    }

    // Piper voice download (on demand — the 63.5 MB model is never
    // bundled). Shown only when Piper is the selected engine.
    if state.draft.voice.engine == TtsEngineKind::Piper {
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
        match &state.piper_voices_dir {
            None => {
                ui.label(
                    egui::RichText::new(
                        "Không xác định được thư mục giọng (thiếu APPDATA) — không tải được giọng Piper.",
                    )
                    .size(12.5)
                    .color(egui::Color32::from_rgb(248, 113, 113)),
                );
            }
            Some(voices_dir) => {
                let voice_id = state.draft.voice.voice.clone();
                let voice = find_piper_voice(&voice_id);
                let downloaded = voice.is_some_and(|v| voice_files_present(v, voices_dir));
                let downloading = state
                    .piper_download
                    .as_ref()
                    .is_some_and(|p| p.voice_id == voice_id && !p.done);
                ui.horizontal(|ui| {
                    if downloaded {
                        ui.label(
                            egui::RichText::new("Giọng đã tải — sẵn sàng dùng (offline).")
                                .size(12.5)
                                .color(egui::Color32::from_rgb(74, 222, 128)),
                        );
                    } else if downloading {
                        if let Some(progress) = &state.piper_download {
                            let fraction = match progress.total {
                                Some(total) if total > 0 => {
                                    (progress.downloaded as f32 / total as f32).clamp(0.0, 1.0)
                                }
                                _ => 0.0,
                            };
                            ui.add(
                                egui::ProgressBar::new(fraction)
                                    .desired_width(220.0)
                                    .text(progress.status.clone()),
                            );
                        }
                    } else if ui.button("Tải giọng").clicked() {
                        state.start_piper_download(voice_id.clone());
                        *emitted = Some(SettingsCommand::DownloadPiperVoice {
                            voice_id: voice_id.clone(),
                        });
                    }
                    if let Some(progress) = &state.piper_download {
                        if progress.voice_id == voice_id && progress.done {
                            ui.label(
                                egui::RichText::new(progress.status.clone())
                                    .size(12.5)
                                    .color(egui::Color32::from_rgb(74, 222, 128)),
                            );
                        } else if progress.voice_id == voice_id
                            && progress.status.starts_with("Lỗi")
                        {
                            ui.label(
                                egui::RichText::new(progress.status.clone())
                                    .size(12.5)
                                    .color(egui::Color32::from_rgb(248, 113, 113)),
                            );
                        }
                    }
                });
                if !downloaded && !downloading {
                    if let Some(voice) = voice {
                        ui.label(
                            egui::RichText::new(format!(
                                "Giọng Piper tải về khi cần ({} MB), lưu tại %APPDATA%\\cap2voice\\voices\\piper\\{} — chỉ dùng cá nhân.",
                                voice.onnx_bytes / 1_048_576,
                                voice.id
                            ))
                            .size(12.0)
                            .color(muted_text_color()),
                        );
                    }
                }
            }
        }
    }

    ui.add_space(10.0);
    ui.checkbox(
        &mut state.draft.voice.auto_read,
        "Tự động đọc phụ đề tiếng Việt",
    );

    ui.add_space(10.0);
    ui.horizontal(|ui| {
        if ui
            .add(IconButton::new(UiIcon::Play))
            .on_hover_text("Nghe thử")
            .clicked()
        {
            *emitted = Some(SettingsCommand::PreviewVoice {
                engine: state.draft.voice.engine,
                voice: state.draft.voice.voice.clone(),
                rate: state.draft.voice.speech_rate.clone(),
                volume: state.draft.voice.volume,
            });
        }
        render_async_feedback(ui, &state.tts_preview_status);
    });
}

fn render_color_row(
    ui: &mut egui::Ui,
    hex_value: &mut String,
    presets: &[(&str, &str)],
    fallback: egui::Color32,
) {
    ui.horizontal(|ui| {
        let preview_color = parse_hex_color(hex_value).unwrap_or(fallback);
        let (swatch_rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
        ui.painter()
            .rect_filled(swatch_rect, egui::CornerRadius::same(4), preview_color);
        ui.add(egui::TextEdit::singleline(hex_value).desired_width(82.0));
        for (preset_hex, label) in presets {
            let is_active = hex_value.eq_ignore_ascii_case(preset_hex);
            let color = parse_hex_color(preset_hex).unwrap_or(fallback);
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::click());
            ui.painter()
                .rect_filled(rect, egui::CornerRadius::same(4), color);
            ui.painter().rect_stroke(
                rect,
                egui::CornerRadius::same(4),
                egui::Stroke::new(
                    if is_active { 2.0 } else { 1.0 },
                    if is_active {
                        egui::Color32::WHITE
                    } else {
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 70)
                    },
                ),
                egui::StrokeKind::Inside,
            );
            if resp
                .on_hover_text(format!("{label}: {preset_hex}"))
                .clicked()
            {
                *hex_value = (*preset_hex).to_string();
            }
        }
    });
}

fn render_display_tab(ui: &mut egui::Ui, state: &mut SettingsDraftState) {
    egui::Grid::new("settings_display_grid")
        .num_columns(2)
        .spacing([18.0, 12.0])
        .show(ui, |ui| {
            ui.label("Cỡ chữ phụ đề");
            ui.add(
                egui::Slider::new(&mut state.draft.display.font_size_px, 12.0..=40.0)
                    .fixed_decimals(0)
                    .suffix(" px"),
            );
            ui.end_row();

            ui.label("Độ mờ nền overlay");
            let mut op_pct = (state.draft.display.overlay_opacity * 100.0).round();
            if ui
                .add(
                    egui::Slider::new(&mut op_pct, 30.0..=100.0)
                        .fixed_decimals(0)
                        .suffix("%"),
                )
                .changed()
            {
                state.draft.display.overlay_opacity = (op_pct / 100.0).clamp(0.30, 1.0);
            }
            ui.end_row();

            ui.label("Màu chữ tiếng Trung");
            render_color_row(
                ui,
                &mut state.draft.display.chinese_color_hex,
                CHINESE_COLOR_PRESETS,
                egui::Color32::WHITE,
            );
            ui.end_row();

            ui.label("Màu chữ tiếng Việt");
            render_color_row(
                ui,
                &mut state.draft.display.vietnamese_color_hex,
                VIETNAMESE_COLOR_PRESETS,
                egui::Color32::from_rgb(74, 222, 128),
            );
            ui.end_row();
        });

    ui.add_space(10.0);
    ui.checkbox(
        &mut state.draft.display.show_chinese_line,
        "Hiển thị dòng tiếng Trung",
    );
    ui.checkbox(
        &mut state.draft.display.show_vietnamese_line,
        "Hiển thị dòng tiếng Việt",
    );
    ui.checkbox(
        &mut state.draft.display.show_pinyin_line,
        "Hiển thị pinyin trên dòng tiếng Trung",
    );
    ui.checkbox(
        &mut state.draft.display.always_on_top,
        "Luôn hiển thị overlay trên cùng",
    );

    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.label("Hiện phụ đề trước đó");
        let history_label = match state.draft.display.history_lines {
            1 => "1 dòng",
            2 => "2 dòng",
            _ => "Tắt",
        };
        egui::ComboBox::from_id_salt("settings_history_lines")
            .selected_text(history_label)
            .width(120.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut state.draft.display.history_lines, 0, "Tắt");
                ui.selectable_value(&mut state.draft.display.history_lines, 1, "1 dòng");
                ui.selectable_value(&mut state.draft.display.history_lines, 2, "2 dòng");
            });
    });
    ui.label(
        egui::RichText::new(
            "Hiện 1–2 cặp phụ đề đã hoàn tất (mờ hơn) phía trên phụ đề hiện tại. \
             Áp dụng ngay khi lưu, không cần khởi động lại.",
        )
        .size(11.5)
        .color(muted_text_color()),
    );

    ui.add_space(10.0);
    ui.checkbox(
        &mut state.draft.display.inplace_subtitle,
        "Hiển thị phụ đề tại vị trí gốc trên video",
    );
    ui.add_enabled_ui(state.draft.display.inplace_subtitle, |ui| {
        ui.horizontal(|ui| {
            ui.label("Độ mờ nền phụ đề tại chỗ");
            let mut opacity_pct = (state.draft.display.inplace_bg_opacity * 100.0).round();
            if ui
                .add(
                    egui::Slider::new(&mut opacity_pct, 0.0..=100.0)
                        .fixed_decimals(0)
                        .suffix("%"),
                )
                .on_hover_text("Độ mờ của nền che phụ đề gốc (0% = trong suốt)")
                .changed()
            {
                state.draft.display.inplace_bg_opacity = (opacity_pct / 100.0).clamp(0.0, 1.0);
            }
        });
    });

    ui.add_space(10.0);
    let alpha = (state.draft.display.overlay_opacity.clamp(0.30, 1.0) * 255.0).round() as u8;
    egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(15, 17, 22, alpha))
        .stroke(egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 36),
        ))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            // Sample history pair (dimmed), shown when the setting is on.
            if state.draft.display.history_lines > 0 {
                let zh_hist = parse_hex_color(&state.draft.display.chinese_color_hex)
                    .unwrap_or(egui::Color32::WHITE);
                let vi_hist = parse_hex_color(&state.draft.display.vietnamese_color_hex)
                    .unwrap_or(egui::Color32::from_rgb(74, 222, 128));
                if state.draft.display.show_chinese_line {
                    ui.label(
                        egui::RichText::new("之前的一句。")
                            .size((state.draft.display.font_size_px * 1.1).clamp(14.0, 28.0))
                            .color(dim_color(zh_hist)),
                    );
                }
                if state.draft.display.show_vietnamese_line {
                    ui.label(
                        egui::RichText::new("Câu trước đó.")
                            .size((state.draft.display.font_size_px * 0.85).clamp(12.0, 22.0))
                            .color(dim_color(vi_hist)),
                    );
                }
            }
            if state.draft.display.show_pinyin_line && state.draft.display.show_chinese_line {
                ui.label(
                    egui::RichText::new("yè shēn le, yǔ hái zài xià。")
                        .size((state.draft.display.font_size_px * 0.8).clamp(11.0, 20.0))
                        .color(egui::Color32::from_rgb(148, 163, 184)),
                );
            }
            if state.draft.display.show_chinese_line {
                let zh_color = parse_hex_color(&state.draft.display.chinese_color_hex)
                    .unwrap_or(egui::Color32::WHITE);
                ui.label(
                    egui::RichText::new("夜深了，雨还在下。")
                        .size((state.draft.display.font_size_px * 1.1).clamp(14.0, 28.0))
                        .color(zh_color),
                );
            }
            if state.draft.display.show_vietnamese_line {
                let vi_color = parse_hex_color(&state.draft.display.vietnamese_color_hex)
                    .unwrap_or(egui::Color32::from_rgb(74, 222, 128));
                ui.label(
                    egui::RichText::new("Đêm đã khuya, mưa vẫn còn rơi.")
                        .size((state.draft.display.font_size_px * 0.85).clamp(12.0, 22.0))
                        .color(vi_color),
                );
            }
        });
}

fn render_hotkeys_tab(ui: &mut egui::Ui, state: &mut SettingsDraftState) {
    egui::Grid::new("settings_hotkeys_grid")
        .num_columns(2)
        .spacing([18.0, 12.0])
        .show(ui, |ui| {
            ui.label("Bắt đầu / Dừng dịch");
            ui.add(
                egui::TextEdit::singleline(&mut state.draft.hotkeys.toggle_start_stop)
                    .desired_width(180.0),
            );
            ui.end_row();

            ui.label("Chọn vùng phụ đề");
            ui.add(
                egui::TextEdit::singleline(&mut state.draft.hotkeys.select_region)
                    .desired_width(180.0),
            );
            ui.end_row();

            ui.label("Hiện / Ẩn overlay");
            ui.add(
                egui::TextEdit::singleline(&mut state.draft.hotkeys.toggle_overlay)
                    .desired_width(180.0),
            );
            ui.end_row();

            ui.label("Tắt / Bật tiếng");
            ui.add(
                egui::TextEdit::singleline(&mut state.draft.hotkeys.toggle_mute)
                    .desired_width(180.0),
            );
            ui.end_row();
        });

    ui.add_space(12.0);
    if ui
        .add(IconButton::new(UiIcon::Reset))
        .on_hover_text("Khôi phục mặc định")
        .clicked()
    {
        state.reset_hotkeys_to_default();
    }
}

/// Render the 5-tab `Cài đặt` window UI and emit at most one user command per frame.
pub fn render_settings_ui(
    ui: &mut egui::Ui,
    state: &mut SettingsDraftState,
) -> Option<SettingsCommand> {
    let cancel_requested =
        ui.input(|i| i.viewport().close_requested() || i.key_pressed(egui::Key::Escape));
    if cancel_requested {
        return Some(SettingsCommand::Cancel);
    }

    let mut emitted: Option<SettingsCommand> = None;

    egui::Panel::left("settings_sidebar_panel")
        .exact_size(164.0)
        .resizable(false)
        .frame(
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(24, 26, 32))
                .inner_margin(egui::Margin::symmetric(10, 14)),
        )
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new("Cài đặt")
                    .size(16.0)
                    .strong()
                    .color(egui::Color32::WHITE),
            );
            ui.add_space(12.0);
            for tab in SettingsTab::ALL {
                let selected = state.active_tab == tab;
                let fill = if selected {
                    accent_blue_color()
                } else {
                    egui::Color32::TRANSPARENT
                };
                let text_color = if selected {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::from_rgb(203, 213, 225)
                };
                let btn = egui::Button::new(
                    egui::RichText::new(tab.label())
                        .size(13.5)
                        .color(text_color),
                )
                .fill(fill)
                .corner_radius(egui::CornerRadius::same(7))
                .min_size(egui::vec2(ui.available_width(), 32.0));
                if ui.add(btn).clicked() {
                    state.active_tab = tab;
                }
                ui.add_space(4.0);
            }
        });

    egui::Panel::bottom("settings_footer_panel")
        .resizable(false)
        .frame(
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(26, 29, 36))
                .inner_margin(egui::Margin::symmetric(16, 10)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if let Some(err) = &state.validation_error {
                    ui.label(
                        egui::RichText::new(err)
                            .size(12.0)
                            .color(egui::Color32::from_rgb(248, 113, 113)),
                    );
                } else if state.is_dirty() {
                    ui.label(
                        egui::RichText::new("Có thay đổi chưa lưu")
                            .size(12.0)
                            .color(egui::Color32::from_rgb(250, 204, 21)),
                    );
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(IconButton::new(UiIcon::Check).filled(accent_blue_color()))
                        .on_hover_text("Lưu thay đổi")
                        .clicked()
                    {
                        if let Ok(committed) = state.try_commit() {
                            emitted = Some(SettingsCommand::Save(Box::new(committed)));
                        }
                    }

                    if ui
                        .add(IconButton::new(UiIcon::Close))
                        .on_hover_text("Huỷ")
                        .clicked()
                    {
                        emitted = Some(SettingsCommand::Cancel);
                    }
                });
            });
        });

    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(30, 33, 40))
                .inner_margin(egui::Margin::symmetric(20, 16)),
        )
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(state.active_tab.label())
                    .size(20.0)
                    .strong()
                    .color(egui::Color32::WHITE),
            );
            ui.label(
                egui::RichText::new(state.active_tab.subtitle())
                    .size(12.5)
                    .color(muted_text_color()),
            );
            ui.add_space(12.0);
            ui.separator();
            ui.add_space(12.0);

            egui::ScrollArea::vertical().show(ui, |ui| match state.active_tab {
                SettingsTab::General => render_general_tab(ui, state, &mut emitted),
                SettingsTab::Translation => render_translation_tab(ui, state, &mut emitted),
                SettingsTab::Voice => render_voice_tab(ui, state, &mut emitted),
                SettingsTab::Display => render_display_tab(ui, state),
                SettingsTab::Hotkeys => render_hotkeys_tab(ui, state),
            });
        });

    emitted
}

#[cfg(test)]
mod tests {
    use super::{
        build_settings_viewport_builder, render_settings_ui, SettingsCommand, SettingsDraftState,
        SettingsTab,
    };
    use crate::config::{AppConfig, CaptureRegion, HotkeyConfig};
    use crate::translate::TranslationEngine;
    use eframe::egui;
    use std::path::PathBuf;

    #[test]
    fn settings_tabs_have_exact_five_vietnamese_labels_in_spec_order() {
        let labels: Vec<&str> = SettingsTab::ALL.iter().map(|tab| tab.label()).collect();
        assert_eq!(
            labels,
            vec!["Chung", "Dịch", "Giọng đọc", "Hiển thị", "Phím tắt"]
        );
    }

    #[test]
    fn settings_draft_discard_returns_original_config_untouched() {
        let mut original = AppConfig::default();
        original.voice.volume = 0.75;
        original.display.font_size_px = 18.0;

        let mut draft_state = SettingsDraftState::from_config(&original);
        draft_state.draft.voice.volume = 0.25;
        draft_state.draft.display.font_size_px = 32.0;
        draft_state.draft.general.capture_fps = 15;
        draft_state.draft.translation.engine = TranslationEngine::Vietphrase;
        assert!(draft_state.is_dirty());

        let discarded = draft_state.discard();
        assert_eq!(discarded, original);
        assert_eq!(draft_state.original(), &original);
    }

    #[test]
    fn settings_draft_commit_validates_and_applies_all_five_tabs_fields() {
        let original = AppConfig::default();
        let mut draft_state = SettingsDraftState::from_config(&original);

        draft_state.draft.general.capture_fps = 15;
        draft_state.wxocr_path_input = r"  C:\Custom\wxocr.dll  ".to_string();
        draft_state.weixin_dir_input = r"C:\Custom\Weixin".to_string();
        draft_state.draft.general.enable_subtitle_log = false;
        draft_state.log_dir_input = r"C:\Logs\cap2voice".to_string();
        draft_state.draft.general.debug_logging = true;
        draft_state.draft.general.close_to_tray = false;
        draft_state.draft.general.start_with_windows = true;
        draft_state.draft.general.start_minimized_to_tray = true;

        draft_state.draft.translation.engine = TranslationEngine::Vietphrase;
        draft_state.draft.translation.dedupe_threshold = 0.88;
        draft_state.draft.translation.pixel_diff_threshold = 0.05;
        draft_state.draft.translation.blank_reset_frames = 8;

        draft_state.draft.voice.engine = crate::tts::TtsEngineKind::Piper;
        draft_state.draft.voice.voice = "ngochuyen".to_string();
        draft_state.draft.voice.volume = 0.45;
        draft_state.draft.voice.speech_rate = "+10%".to_string();
        draft_state.draft.voice.auto_read = false;

        draft_state.draft.display.font_size_px = 24.0;
        draft_state.draft.display.overlay_opacity = 0.65;
        draft_state.draft.display.show_chinese_line = false;
        draft_state.draft.display.always_on_top = false;
        draft_state.draft.display.chinese_color_hex = "#e2e8f0".to_string();
        draft_state.draft.display.vietnamese_color_hex = "#38bdf8".to_string();

        draft_state.draft.hotkeys.toggle_start_stop = "Ctrl+Shift+S".to_string();

        let committed = match draft_state.try_commit() {
            Ok(cfg) => cfg,
            Err(err) => panic!("expected valid SettingsDraftState commit, got {err}"),
        };
        assert_eq!(committed.general.capture_fps, 15);
        assert_eq!(
            committed.general.wxocr_dll_path,
            Some(PathBuf::from(r"C:\Custom\wxocr.dll"))
        );
        assert_eq!(
            committed.general.weixin_dir_path,
            Some(PathBuf::from(r"C:\Custom\Weixin"))
        );
        assert!(!committed.general.enable_subtitle_log);
        assert_eq!(
            committed.general.subtitle_log_dir,
            Some(PathBuf::from(r"C:\Logs\cap2voice"))
        );
        assert!(committed.general.debug_logging);
        assert!(!committed.general.close_to_tray);
        assert!(committed.general.start_with_windows);
        assert!(committed.general.start_minimized_to_tray);
        assert_eq!(committed.translation.engine, TranslationEngine::Vietphrase);
        assert!((committed.translation.dedupe_threshold - 0.88).abs() < 1e-6);
        assert!((committed.translation.pixel_diff_threshold - 0.05).abs() < 1e-6);
        assert_eq!(committed.translation.blank_reset_frames, 8);
        assert_eq!(committed.voice.engine, crate::tts::TtsEngineKind::Piper);
        assert_eq!(committed.voice.voice, "ngochuyen");
        assert!((committed.voice.volume - 0.45).abs() < 1e-6);
        assert_eq!(committed.voice.speech_rate, "+10%");
        assert!(!committed.voice.auto_read);
        assert!((committed.display.font_size_px - 24.0).abs() < 1e-6);
        assert!((committed.display.overlay_opacity - 0.65).abs() < 1e-6);
        assert!(!committed.display.show_chinese_line);
        assert!(!committed.display.always_on_top);
        assert_eq!(committed.display.chinese_color_hex, "#E2E8F0");
        assert_eq!(committed.display.vietnamese_color_hex, "#38BDF8");
        assert_eq!(committed.hotkeys.toggle_start_stop, "Ctrl+Shift+S");
        assert_eq!(draft_state.validation_error, None);
    }

    #[test]
    fn settings_draft_rejects_invalid_hex_colors_and_duplicate_or_empty_hotkeys() {
        let cfg = AppConfig::default();

        // Malformed hex color
        let mut state = SettingsDraftState::from_config(&cfg);
        state.draft.display.vietnamese_color_hex = "#12ZZ00".to_string();
        assert!(state.try_commit().is_err());
        assert!(state.validation_error.is_some());

        // Empty hotkey
        let mut state = SettingsDraftState::from_config(&cfg);
        state.draft.hotkeys.toggle_mute = "   ".to_string();
        assert!(state.try_commit().is_err());
        assert!(state.validation_error.is_some());

        // Duplicate hotkey (case-insensitive)
        let mut state = SettingsDraftState::from_config(&cfg);
        state.draft.hotkeys.toggle_overlay = "ctrl+alt+s".to_string();
        assert!(state.try_commit().is_err());
        assert!(state.validation_error.is_some());
    }

    #[test]
    fn settings_draft_select_region_updates_draft_only_without_mutating_original() {
        let initial_region = CaptureRegion {
            monitor_id: r"\\.\DISPLAY1".to_string(),
            x: 100,
            y: 900,
            width: 800,
            height: 60,
        };
        let mut cfg = AppConfig::default();
        cfg.general.region = Some(initial_region.clone());

        let mut draft_state = SettingsDraftState::from_config(&cfg);
        let new_region = CaptureRegion {
            monitor_id: r"\\.\DISPLAY1".to_string(),
            x: 420,
            y: 1020,
            width: 1280,
            height: 60,
        };
        draft_state.set_draft_region(new_region.clone());

        assert_eq!(draft_state.draft.general.region, Some(new_region.clone()));
        assert_eq!(draft_state.original().general.region, Some(initial_region));
        assert!(draft_state.is_dirty());

        let committed = match draft_state.try_commit() {
            Ok(c) => c,
            Err(err) => panic!("expected commit to succeed: {err}"),
        };
        assert_eq!(committed.general.region, Some(new_region));
    }

    #[test]
    fn settings_draft_reset_hotkeys_restores_spec_defaults() {
        let mut draft_state = SettingsDraftState::from_config(&AppConfig::default());
        draft_state.draft.hotkeys = HotkeyConfig {
            toggle_start_stop: "F6".to_string(),
            select_region: "F7".to_string(),
            toggle_overlay: "F8".to_string(),
            toggle_mute: "F9".to_string(),
        };

        draft_state.reset_hotkeys_to_default();
        assert_eq!(draft_state.draft.hotkeys, HotkeyConfig::default());
    }

    #[test]
    fn build_settings_viewport_builder_configures_decorated_opaque_settings_window() {
        let vb = build_settings_viewport_builder();
        assert_eq!(vb.title.as_deref(), Some("Cài đặt"));
        assert_eq!(vb.decorations, Some(true));
        assert_eq!(vb.transparent, Some(false));
        assert_eq!(vb.resizable, Some(true));
        assert_eq!(vb.active, Some(true));
    }

    #[test]
    fn render_settings_ui_draws_all_five_tabs_headlessly_without_panic_and_emits_cancel_on_esc() {
        let mut draft_state = SettingsDraftState::from_config(&AppConfig::default());
        let ctx = egui::Context::default();
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(680.0, 480.0));

        for tab in SettingsTab::ALL {
            draft_state.active_tab = tab;
            let input = egui::RawInput {
                screen_rect: Some(screen_rect),
                ..Default::default()
            };
            let output = ctx.run_ui(input, |ui| {
                let cmd = render_settings_ui(ui, &mut draft_state);
                assert_eq!(cmd, None);
            });
            assert!(
                !output.shapes.is_empty(),
                "expected shapes rendered for tab {:?}",
                tab
            );
        }

        let esc_input = egui::RawInput {
            screen_rect: Some(screen_rect),
            events: vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: Some(egui::Key::Escape),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let mut esc_cmd = None;
        let _ = ctx.run_ui(esc_input, |ui| {
            esc_cmd = render_settings_ui(ui, &mut draft_state);
        });
        assert_eq!(esc_cmd, Some(SettingsCommand::Cancel));
    }
}

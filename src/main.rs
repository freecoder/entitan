use eframe::egui;
use notify::{RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};
use rfd::FileDialog;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

// The two entries in Config.wtf that set game language
// SET audioLocale "enUS"
// SET textLocale "enUS"

fn main() {
    // Load settings to read any saved window geometry (position & size)
    let (_battle, _config, _wow, _preferred, geom) = load_settings_full();

    // Single-instance enforcement: lock a file in the settings directory (or temp dir)
    use fs2::FileExt;
    use std::fs::OpenOptions;

    let lock_path = settings_file_path()
        .and_then(|p| p.parent().map(|d| d.join("entitan.lock")))
        .unwrap_or_else(|| std::env::temp_dir().join("entitan.lock"));
    if let Some(parent) = lock_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let lock_file = match OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Failed to create lock file {}: {}", lock_path.display(), e);
            return;
        }
    };
    if let Err(_) = lock_file.try_lock_exclusive() {
        // Another instance is running — show a dialog and exit
        let _ = rfd::MessageDialog::new()
            .set_title("enTitan already running")
            .set_description("Another instance of enTitan is already running.")
            .set_buttons(rfd::MessageButtons::Ok)
            .show();
        return;
    }
    // Keep the lock file alive for the lifetime of main so the lock remains held
    let _lock_file = lock_file;

    let mut options = eframe::NativeOptions::default();
    // Minimum window size (enforced where supported)
    let min_size = egui::vec2(600.0, 400.0);
    options.viewport.min_inner_size = Some(min_size);

    // Default initial size if no saved geometry
    let default_size = min_size;

    // Use ViewportBuilder but make sure to set min_inner_size on the builder so it isn't lost
    let mut vp_builder = egui::viewport::ViewportBuilder::default().with_min_inner_size(min_size);
    if let Some((x, y, w, h)) = geom {
        vp_builder = vp_builder
            .with_inner_size(egui::vec2(w, h))
            .with_position(egui::pos2(x as f32, y as f32));
    } else {
        vp_builder = vp_builder.with_inner_size(default_size);
    }
    options.viewport = vp_builder;

    let _ = eframe::run_native(
        "enTitan - Titan Reforged Locale Launcher",
        options,
        Box::new(|_cc| Ok(Box::new(EntitanApp::default()))),
    );
}

struct EntitanApp {
    battle_net_path: String,
    config_wtf_path: String,
    wow_executable_path: String,
    status: Option<String>,
    // Preferred locale editable by the user (persisted)
    preferred_locale: String,
    // Cached values parsed from the Config.wtf file (if available)
    audio_locale: Option<String>,
    text_locale: Option<String>,
    last_config_path: Option<String>,
    // File watcher (notify)
    watcher: Option<RecommendedWatcher>,
    watcher_rx: Option<std::sync::mpsc::Receiver<notify::Result<notify::Event>>>,
    // Background image texture (loaded from ./background.png)
    background_texture: Option<egui::TextureHandle>,
    background_size: Option<[usize; 2]>,
    background_load_attempted: bool,
    // Cache of last seen inner size and window position (updated each frame)
    last_inner_size: Option<(f32, f32)>,
    last_window_pos: Option<(i32, i32)>,
    // Run sequence state
    run_active: bool,
    run_tx: std::sync::mpsc::Sender<String>,
    run_rx: std::sync::mpsc::Receiver<String>,
}

impl Default for EntitanApp {
    fn default() -> Self {
        let (battle, config, wow, preferred, _geom) = load_settings_full();
        let (tx, rx) = std::sync::mpsc::channel();

        // Create file watcher (notify) to get OS-level notifications for Config.wtf changes
        let (watch_tx, watch_rx) = std::sync::mpsc::channel();
        let watcher = match recommended_watcher(move |res| {
            let _ = watch_tx.send(res);
        }) {
            Ok(mut w) => {
                if !config.is_empty() {
                    if Path::new(&config).exists() {
                        let _ = w.watch(Path::new(&config), RecursiveMode::NonRecursive);
                    }
                }
                Some(w)
            }
            Err(e) => {
                eprintln!("Failed to create file watcher: {}", e);
                None
            }
        };

        Self {
            battle_net_path: battle,
            config_wtf_path: config,
            wow_executable_path: wow,
            status: None,
            preferred_locale: if preferred.is_empty() {
                "enUS".into()
            } else {
                preferred
            },
            audio_locale: None,
            text_locale: None,
            last_config_path: None,
            watcher: watcher,
            watcher_rx: Some(watch_rx),
            background_texture: None,
            background_size: None,
            background_load_attempted: false,
            last_inner_size: None,
            last_window_pos: None,
            run_active: false,
            run_tx: tx,
            run_rx: rx,
        }
    }
}

impl EntitanApp {
    /// Update cached `audio_locale` and `text_locale` if the config path changed.
    fn update_locales(&mut self) {
        let cfg = self.config_wtf_path.clone();
        // Only re-run parsing when the path changed
        if self.last_config_path.as_ref() == Some(&cfg) {
            return;
        }
        let old_path = self.last_config_path.clone();
        self.last_config_path = if cfg.is_empty() {
            None
        } else {
            Some(cfg.clone())
        };

        // Update watcher registration if present
        if let Some(ref mut watcher) = self.watcher {
            if let Some(old) = old_path {
                let _ = watcher.unwatch(Path::new(&old));
            }
            if !cfg.is_empty() {
                let _ = watcher.watch(Path::new(&cfg), RecursiveMode::NonRecursive);
            }
        }

        self.audio_locale = None;
        self.text_locale = None;

        if cfg.is_empty() {
            return;
        }
        let p = Path::new(&cfg);
        if !p.exists() {
            // leave as None
            return;
        }
        if let Ok(meta) = p.metadata() {
            if meta.len() >= 8192 {
                // File too large — don't open
                self.audio_locale = Some("(file too large)".into());
                self.text_locale = Some("(file too large)".into());
                return;
            }
        }
        if let Ok(contents) = fs::read_to_string(p) {
            for line in contents.lines() {
                let s = line.trim();
                if s.starts_with("SET audioLocale") {
                    if let Some(first) = s.find('"') {
                        let rest = &s[first + 1..];
                        if let Some(end) = rest.find('"') {
                            self.audio_locale = Some(rest[..end].to_string());
                        }
                    }
                } else if s.starts_with("SET textLocale") {
                    if let Some(first) = s.find('"') {
                        let rest = &s[first + 1..];
                        if let Some(end) = rest.find('"') {
                            self.text_locale = Some(rest[..end].to_string());
                        }
                    }
                }
            }
        }
    }

    /// Update both `SET audioLocale` and `SET textLocale` lines in the Config.wtf file
    /// to match `self.preferred_locale`. Performs existence and size checks (<8192 bytes).
    fn update_config_file_locales(&mut self) -> Result<(), String> {
        let cfg = self.config_wtf_path.clone();
        if cfg.is_empty() {
            return Err("Config.wtf path is not set".into());
        }
        let p = Path::new(&cfg);
        if !p.exists() || !p.is_file() {
            return Err("Config.wtf path does not exist or is not a file".into());
        }
        let meta = p.metadata().map_err(|e| e.to_string())?;
        if meta.len() >= 8192 {
            return Err("Config.wtf file is too large to safely edit".into());
        }
        let contents = fs::read_to_string(p).map_err(|e| e.to_string())?;
        let mut lines: Vec<String> = contents.lines().map(|l| l.to_string()).collect();
        let mut found_audio = false;
        let mut found_text = false;
        for line in lines.iter_mut() {
            let s = line.trim();
            if s.starts_with("SET audioLocale") {
                *line = format!("SET audioLocale \"{}\"", self.preferred_locale);
                found_audio = true;
            } else if s.starts_with("SET textLocale") {
                *line = format!("SET textLocale \"{}\"", self.preferred_locale);
                found_text = true;
            }
        }
        if !found_audio {
            lines.push(format!("SET audioLocale \"{}\"", self.preferred_locale));
        }
        if !found_text {
            lines.push(format!("SET textLocale \"{}\"", self.preferred_locale));
        }
        let mut out = lines.join("\n");
        out.push('\n');
        fs::write(p, out).map_err(|e| e.to_string())?;
        // Force a refresh of cached values even if the file path didn't change
        self.last_config_path = None;
        self.update_locales();
        Ok(())
    }
}

impl eframe::App for EntitanApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            // Try to load background.png once (from current working directory)
            if !self.background_load_attempted && self.background_texture.is_none() {
                self.background_load_attempted = true;
                let bg_path = std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join("background.png");
                if bg_path.exists() {
                    match image::open(&bg_path) {
                        Ok(img) => {
                            // convert to RGBA8 and then to grayscale with 10% opacity
                            let img = img.to_rgba8();
                            let w = img.width() as usize;
                            let h = img.height() as usize;
                            let mut pixels = img.into_vec();
                            for chunk in pixels.chunks_exact_mut(4) {
                                let r = chunk[0] as f32;
                                let g = chunk[1] as f32;
                                let b = chunk[2] as f32;
                                let a = chunk[3];
                                // luminance per Rec. 601
                                let lum = (0.299 * r + 0.587 * g + 0.114 * b).round() as u8;
                                chunk[0] = lum;
                                chunk[1] = lum;
                                chunk[2] = lum;
                                // set opacity to 10% of original
                                chunk[3] = ((a as f32) * 0.1).round() as u8;
                            }
                            let size = [w, h];
                            let color_image =
                                egui::ColorImage::from_rgba_unmultiplied(size, &pixels);
                            let tex = ctx.load_texture(
                                "background",
                                color_image,
                                egui::TextureOptions::LINEAR,
                            );
                            self.background_texture = Some(tex);
                            self.background_size = Some([w, h]);
                        }
                        Err(e) => {
                            self.status = Some(format!("Failed to load background.png: {}", e));
                        }
                    }
                }
            }

            // Paint background if we have it (preserve aspect ratio, cover, center crop)
            if let Some(ref tex) = self.background_texture {
                let rect = ui.max_rect();
                if let Some([img_w, img_h]) = self.background_size {
                    let img_w_f = img_w as f32;
                    let img_h_f = img_h as f32;
                    let rect_w = rect.width();
                    let rect_h = rect.height();
                    // scale so the image covers the rect
                    let scale = f32::max(rect_w / img_w_f, rect_h / img_h_f);
                    // visible size in texture pixels
                    let visible_w = rect_w / scale;
                    let visible_h = rect_h / scale;
                    let u0 = ((img_w_f - visible_w) / 2.0) / img_w_f;
                    let v0 = ((img_h_f - visible_h) / 2.0) / img_h_f;
                    let u1 = u0 + visible_w / img_w_f;
                    let v1 = v0 + visible_h / img_h_f;
                    let uv_rect = egui::Rect::from_min_max(egui::pos2(u0, v0), egui::pos2(u1, v1));
                    ui.painter()
                        .image(tex.id(), rect, uv_rect, egui::Color32::WHITE);
                } else {
                    ui.painter().image(
                        tex.id(),
                        rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                }
            }

            // refresh cached locales if config path changed
            self.update_locales();

            // update cached window geometry (so we can save on close without access to frame later)
            let size = ctx.input(|i| i.content_rect().size());
            self.last_inner_size = Some((size.x, size.y));
            // update last_window_pos each frame too
            self.last_window_pos = get_window_position(_frame);

            ui.vertical(|ui| {
                // Top labels for game language (left-aligned and not stretched)
                let label_w = 140.0;
                let btn_w = 80.0;
                let gap = 6.0;
                let right_pad = 8.0; // reserve an explicit right padding for buttons below
                let total_avail = ui.available_width();
                let btn_count_max = 2.0; // reserve for up to two buttons (Browse + Run)
                let text_w =
                    (total_avail - label_w - btn_w * btn_count_max - gap - right_pad).max(8.0);

                // audioLocale row (aligned and colored; value left-aligned to textfield column)
                ui.horizontal(|ui| {
                    ui.add_sized([label_w, 24.0], egui::Label::new("audioLocale:"));
                    let a = self.audio_locale.as_deref().unwrap_or("(not available)");
                    let a_color = if self
                        .audio_locale
                        .as_deref()
                        .map(|v| v.eq_ignore_ascii_case(&self.preferred_locale))
                        .unwrap_or(false)
                    {
                        egui::Color32::from_rgb(0, 160, 0)
                    } else {
                        egui::Color32::from_rgb(200, 0, 0)
                    };
                    {
                        let (rect, _resp) =
                            ui.allocate_exact_size(egui::vec2(text_w, 24.0), egui::Sense::hover());
                        let pos = rect.left_center();
                        ui.painter().text(
                            pos + egui::vec2(4.0, 0.0),
                            egui::Align2::LEFT_CENTER,
                            a,
                            egui::TextStyle::Body.resolve(ui.style()),
                            a_color,
                        );
                    }
                });

                // textLocale row (aligned and colored; value left-aligned to textfield column)
                ui.horizontal(|ui| {
                    ui.add_sized([label_w, 24.0], egui::Label::new("textLocale:"));
                    let t = self.text_locale.as_deref().unwrap_or("(not available)");
                    let t_color = if self
                        .text_locale
                        .as_deref()
                        .map(|v| v.eq_ignore_ascii_case(&self.preferred_locale))
                        .unwrap_or(false)
                    {
                        egui::Color32::from_rgb(0, 160, 0)
                    } else {
                        egui::Color32::from_rgb(200, 0, 0)
                    };
                    {
                        let (rect, _resp) =
                            ui.allocate_exact_size(egui::vec2(text_w, 24.0), egui::Sense::hover());
                        let pos = rect.left_center();
                        ui.painter().text(
                            pos + egui::vec2(4.0, 0.0),
                            egui::Align2::LEFT_CENTER,
                            t,
                            egui::TextStyle::Body.resolve(ui.style()),
                            t_color,
                        );
                    }
                });

                ui.separator();
                ui.add_space(6.0);

                // Preferred Locale row (aligned)
                ui.horizontal(|ui| {
                    // reuse label_w, btn_w, text_w from above
                    ui.add_sized([label_w, 24.0], egui::Label::new("Preferred Locale:"));
                    ui.add_sized(
                        [text_w, 24.0],
                        egui::TextEdit::singleline(&mut self.preferred_locale),
                    );
                    if ui
                        .add_sized([btn_w, 24.0], egui::Button::new("Update"))
                        .clicked()
                    {
                        match self.update_config_file_locales() {
                            Ok(()) => self.status = Some("Config.wtf updated".into()),
                            Err(e) => self.status = Some(format!("Error updating config: {}", e)),
                        }
                    }
                    // reserve space for a potential second button so alignment matches WoW row
                    ui.add_sized([btn_w, 24.0], egui::Label::new(""));
                    ui.add_sized([right_pad, 24.0], egui::Label::new(""));
                });
                // Enforce only ASCII letters and max length 4; reset invalid values to enUS
                let orig_pref = self.preferred_locale.clone();
                let filtered: String = orig_pref
                    .chars()
                    .filter(|c| c.is_ascii_alphabetic())
                    .take(4)
                    .collect();
                if filtered.is_empty() {
                    // If user cleared the field, keep default; if it's invalid (e.g., geometry), reset and warn
                    if orig_pref.is_empty() {
                        self.preferred_locale = "enUS".into();
                    } else {
                        self.preferred_locale = "enUS".into();
                        self.status = Some("Preferred locale invalid; reset to enUS".into());
                    }
                } else if filtered != orig_pref {
                    self.preferred_locale = filtered;
                    self.status = Some("Preferred locale filtered to letters only (max 4)".into());
                }

                ui.add_space(6.0);

                // Battle.net row (aligned)
                ui.horizontal(|ui| {
                    // reuse label_w, btn_w, text_w from above
                    ui.add_sized([label_w, 24.0], egui::Label::new("Battle.net"));
                    ui.add_sized(
                        [text_w, 24.0],
                        egui::TextEdit::singleline(&mut self.battle_net_path),
                    );
                    if ui
                        .add_sized([btn_w, 24.0], egui::Button::new("Browse"))
                        .clicked()
                    {
                        let mut dialog = FileDialog::new();
                        if !self.battle_net_path.is_empty() {
                            if let Some(parent) = Path::new(&self.battle_net_path).parent() {
                                dialog = dialog.set_directory(parent);
                            }
                        }
                        if let Some(file) = dialog.add_filter("exe", &["exe"]).pick_file() {
                            if is_file_with_ext(&file, "exe") {
                                self.battle_net_path = file.display().to_string();
                                self.status = Some("Selected (unsaved)".into());
                            } else {
                                self.status = Some("Selected file is not an .exe".into());
                            }
                        }
                    }
                    // reserve space for a second button so buttons align across rows
                    ui.add_sized([btn_w, 24.0], egui::Label::new(""));
                    ui.add_sized([right_pad, 24.0], egui::Label::new(""));
                });

                ui.add_space(6.0);

                // Config.wtf row (aligned)
                ui.horizontal(|ui| {
                    // reuse label_w, btn_w, text_w from above
                    ui.add_sized([label_w, 24.0], egui::Label::new("Config.wtf:"));
                    ui.add_sized(
                        [text_w, 24.0],
                        egui::TextEdit::singleline(&mut self.config_wtf_path),
                    );
                    if ui
                        .add_sized([btn_w, 24.0], egui::Button::new("Browse"))
                        .clicked()
                    {
                        let mut dialog = FileDialog::new();
                        if !self.config_wtf_path.is_empty() {
                            if let Some(parent) = Path::new(&self.config_wtf_path).parent() {
                                dialog = dialog.set_directory(parent);
                            }
                        }
                        if let Some(file) = dialog.add_filter("wtf", &["wtf"]).pick_file() {
                            if is_file_with_ext(&file, "wtf") {
                                self.config_wtf_path = file.display().to_string();
                                self.status = Some("Selected (unsaved)".into());
                                // refresh cached locale values immediately
                                self.update_locales();
                            } else {
                                self.status = Some("Selected file is not a .wtf file".into());
                            }
                        }
                    }
                    // reserve space for a second button so buttons align across rows
                    ui.add_sized([btn_w, 24.0], egui::Label::new(""));
                    ui.add_sized([right_pad, 24.0], egui::Label::new(""));
                });

                ui.add_space(6.0);

                // WoW Executable row (aligned)
                ui.horizontal(|ui| {
                    ui.add_sized([label_w, 24.0], egui::Label::new("WoW Executable:"));
                    ui.add_sized(
                        [text_w, 24.0],
                        egui::TextEdit::singleline(&mut self.wow_executable_path),
                    );
                    if ui
                        .add_sized([btn_w, 24.0], egui::Button::new("Browse"))
                        .clicked()
                    {
                        let mut dialog = FileDialog::new();
                        if !self.wow_executable_path.is_empty() {
                            if let Some(parent) = Path::new(&self.wow_executable_path).parent() {
                                dialog = dialog.set_directory(parent);
                            }
                        }
                        if let Some(file) = dialog.add_filter("exe", &["exe"]).pick_file() {
                            if is_file_with_ext(&file, "exe") {
                                self.wow_executable_path = file.display().to_string();
                                self.status = Some("Selected (unsaved)".into());
                            } else {
                                self.status = Some("Selected file is not an .exe".into());
                            }
                        }
                    }
                    ui.add_sized([right_pad, 24.0], egui::Label::new(""));
                });
            });

            ui.separator();
            ui.add_space(12.0);

            // If window is smaller than 600x400, show a warning
            let screen_size = ctx.input(|i| i.content_rect().size());
            let too_small = screen_size.x < 600.0 || screen_size.y < 400.0;
            if too_small {
                ui.colored_label(
                    egui::Color32::from_rgb(200, 0, 0),
                    "Window too small — enlarge to at least 600×400",
                );
                ui.add_space(6.0);
            }

            // Bottom buttons (Run placed left of Close)
            ui.horizontal(|ui| {
                // Run button starts the launch sequence (disabled while active)
                let run_btn = ui.add_enabled(
                    !self.run_active,
                    egui::Button::new("Run").min_size(egui::vec2(80.0, 24.0)),
                );
                if run_btn.clicked() {
                    // validate paths first
                    let p1 = Path::new(&self.battle_net_path);
                    let p2 = Path::new(&self.wow_executable_path);
                    if !(p1.exists() && is_file_with_ext(p1, "exe")) {
                        self.status = Some("Battle.net path must point to an existing .exe".into());
                    } else if !(p2.exists() && is_file_with_ext(p2, "exe")) {
                        self.status = Some("WoW Executable must point to an existing .exe".into());
                    } else {
                        // set run_active, make window topmost, and spawn worker thread
                        self.run_active = true;
                        self.status = Some("Starting run sequence...".into());
                        // Restore window if minimized and then attempt to set window topmost (best-effort)
                        let _ = set_window_minimized(_frame, false);
                        let _ = set_window_topmost(_frame, true);
                        let tx = self.run_tx.clone();
                        let battle_path = self.battle_net_path.clone();
                        let wow_path = self.wow_executable_path.clone();
                        std::thread::spawn(move || {
                            use std::process::Command;
                            use std::thread::sleep;
                            use std::time::Duration;

                            if let Err(e) = Command::new(&battle_path).spawn() {
                                let _ = tx.send(format!("Failed to launch Battle.net: {}", e));
                                let _ = tx.send("FINISHED".into());
                                return;
                            } else {
                                let _ = tx.send("Launched Battle.net".into());
                            }

                            // 10-second countdown, send per-second updates
                            for rem in (1..=10).rev() {
                                let _ = tx.send(format!("Waiting to launch WoW: {}s", rem));
                                sleep(Duration::from_secs(1));
                            }

                            if let Err(e) = Command::new(&wow_path).spawn() {
                                let _ = tx.send(format!("Failed to launch WoW: {}", e));
                                let _ = tx.send("FINISHED".into());
                                return;
                            } else {
                                let _ = tx.send("Launched WoW".into());
                            }

                            // 60-second countdown with per-second updates
                            for rem in (1..=60).rev() {
                                let _ = tx.send(format!(
                                    "Waiting before re-launching Battle.net: {}s",
                                    rem
                                ));
                                sleep(Duration::from_secs(1));
                            }

                            if let Err(e) = Command::new(&battle_path).spawn() {
                                let _ =
                                    tx.send(format!("Failed to launch Battle.net (second): {}", e));
                            } else {
                                let _ = tx.send("Launched Battle.net (second)".into());
                            }

                            let _ = tx.send("FINISHED".into());
                        });
                    }
                }
                ui.add_space(8.0);
                if ui
                    .add_sized([80.0, 24.0], egui::Button::new("Close"))
                    .clicked()
                {
                    let p1 = Path::new(&self.battle_net_path);
                    let p2 = Path::new(&self.config_wtf_path);
                    let p3 = Path::new(&self.wow_executable_path);
                    if p1.exists()
                        && is_file_with_ext(p1, "exe")
                        && p2.exists()
                        && is_file_with_ext(p2, "wtf")
                        && p3.exists()
                        && is_file_with_ext(p3, "exe")
                    {
                        // Use cached geometry
                        let pos_opt = self.last_window_pos;
                        let size_opt = self.last_inner_size;
                        if let Err(e) = save_settings(
                            &self.battle_net_path,
                            &self.config_wtf_path,
                            &self.wow_executable_path,
                            &self.preferred_locale,
                            pos_opt,
                            size_opt,
                        ) {
                            self.status = Some(format!("Error saving: {}", e));
                        } else {
                            std::process::exit(0);
                        }
                    } else {
                        let mut msgs = vec![];
                        if !(p1.exists() && is_file_with_ext(p1, "exe")) {
                            msgs.push("Battle.net path must point to an existing .exe");
                        }
                        if !(p2.exists() && is_file_with_ext(p2, "wtf")) {
                            msgs.push("Config.wtf path must point to an existing .wtf file");
                        }
                        if !(p3.exists() && is_file_with_ext(p3, "exe")) {
                            msgs.push("WoW Executable must point to an existing .exe file");
                        }
                        self.status = Some(msgs.join("; ").into());
                    }
                }
            });

            // Drain run-thread messages to update status and handle finish events
            while let Ok(msg) = self.run_rx.try_recv() {
                if msg == "FINISHED" {
                    self.run_active = false;
                    // clear topmost
                    set_window_topmost(_frame, false);
                    // minimize the window when the run completes (best-effort, Windows-only)
                    let _ = set_window_minimized(_frame, true);
                    self.status = Some("Run sequence completed".into());
                } else {
                    self.status = Some(msg);
                }
            }

            // Drain file watcher events and reload config if our Config.wtf changed
            if let Some(ref rx) = self.watcher_rx {
                // First, drain any outstanding events into a local buffer so we don't hold an immutable
                // borrow of `rx` while we call methods that need a mutable borrow of `self`.
                let mut events = Vec::new();
                while let Ok(res) = rx.try_recv() {
                    events.push(res);
                }
                for res in events {
                    match res {
                        Ok(event) => {
                            for path in event.paths {
                                if !self.config_wtf_path.is_empty() {
                                    if Path::new(&self.config_wtf_path) == path.as_path() {
                                        // Force refresh immediately
                                        self.last_config_path = None;
                                        self.update_locales();
                                        self.status =
                                            Some("Config.wtf changed on disk; reloaded".into());
                                        ctx.request_repaint();
                                        break;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            self.status = Some(format!("File watcher error: {}", e));
                        }
                    }
                }
            }

            // If a run is active, request repaint every second so countdown messages update even without user input
            if self.run_active {
                ctx.request_repaint_after(std::time::Duration::from_secs(1));
            }

            if let Some(ref s) = self.status {
                ui.add_space(6.0);
                ui.label(s);
            }
        });
    }

    // Called when eframe wants to save app state (on shutdown or periodically)
    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        // attempt to save using cached geometry
        let _ = save_settings(
            &self.battle_net_path,
            &self.config_wtf_path,
            &self.wow_executable_path,
            &self.preferred_locale,
            self.last_window_pos,
            self.last_inner_size,
        );
    }

    // Called once on exit; ensure we persist settings here as a fallback
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = save_settings(
            &self.battle_net_path,
            &self.config_wtf_path,
            &self.wow_executable_path,
            &self.preferred_locale,
            self.last_window_pos,
            self.last_inner_size,
        );
    }
}

fn settings_file_path() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        env::var("APPDATA")
            .ok()
            .map(|a| PathBuf::from(a).join("entitan").join("settings.txt"))
    } else {
        if let Ok(xdg) = env::var("XDG_CONFIG_HOME") {
            Some(PathBuf::from(xdg).join("entitan").join("settings.txt"))
        } else if let Ok(home) = env::var("HOME") {
            Some(
                PathBuf::from(home)
                    .join(".config")
                    .join("entitan")
                    .join("settings.txt"),
            )
        } else {
            None
        }
    }
}

// Loads battle, config, wow, preferred locale and optional geometry (x,y,w,h)
fn load_settings_full() -> (String, String, String, String, Option<(i32, i32, f32, f32)>) {
    let path = match settings_file_path() {
        Some(p) => p,
        None => {
            return (
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                None,
            );
        }
    };
    if path.exists() {
        if let Ok(contents) = fs::read_to_string(path) {
            let mut lines = contents.lines();
            let battle = lines.next().unwrap_or("").trim().to_string();
            let config = lines.next().unwrap_or("").trim().to_string();
            let wow = lines.next().unwrap_or("").trim().to_string();
            let preferred = lines.next().unwrap_or("enUS").trim().to_string();
            let geom = lines.next().and_then(|s| {
                let s = s.trim();
                if s.is_empty() {
                    return None;
                }
                let parts: Vec<&str> = s.split(',').collect();
                if parts.len() == 4 {
                    if let (Ok(x), Ok(y), Ok(w), Ok(h)) = (
                        parts[0].parse::<i32>(),
                        parts[1].parse::<i32>(),
                        parts[2].parse::<f32>(),
                        parts[3].parse::<f32>(),
                    ) {
                        return Some((x, y, w, h));
                    }
                }
                None
            });
            (battle, config, wow, preferred, geom)
        } else {
            (
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                None,
            )
        }
    } else {
        (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            None,
        )
    }
}

fn save_settings(
    battle: &str,
    config: &str,
    wow: &str,
    preferred: &str,
    position: Option<(i32, i32)>,
    size: Option<(f32, f32)>,
) -> std::io::Result<()> {
    let path = settings_file_path().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Other, "cannot determine settings path")
    })?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::File::create(path)?;
    write!(file, "{}\n{}\n{}\n{}\n", battle, config, wow, preferred)?;
    if let (Some((x, y)), Some((w, h))) = (position, size) {
        write!(file, "{},{},{},{}\n", x, y, w, h)?;
    } else {
        write!(file, "\n")?;
    }
    Ok(())
}

fn is_file_with_ext(path: impl AsRef<Path>, ext: &str) -> bool {
    let p = path.as_ref();
    p.is_file()
        && p.extension()
            .and_then(|s| s.to_str())
            .map(|e| e.eq_ignore_ascii_case(ext))
            .unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn get_window_position(frame: &eframe::Frame) -> Option<(i32, i32)> {
    use raw_window_handle::HasWindowHandle;
    use raw_window_handle::RawWindowHandle;
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;

    // Use the new HasWindowHandle API
    if let Ok(handle) = frame.window_handle() {
        let raw: raw_window_handle::RawWindowHandle = handle.into();
        if let RawWindowHandle::Win32(win) = raw {
            // hwnd is NonZeroIsize
            let hwnd = win.hwnd.get() as windows_sys::Win32::Foundation::HWND;
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            let ok = unsafe { GetWindowRect(hwnd, &mut rect as *mut RECT) };
            if ok != 0 {
                return Some((rect.left, rect.top));
            }
        }
    }
    None
}

// Best-effort: set or clear always-on-top for our window (Windows only)
fn set_window_topmost(frame: &eframe::Frame, topmost: bool) -> bool {
    #[cfg(target_os = "windows")]
    {
        use raw_window_handle::HasWindowHandle;
        use raw_window_handle::RawWindowHandle;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos,
        };

        // Use the new HasWindowHandle API
        if let Ok(handle) = frame.window_handle() {
            let raw: raw_window_handle::RawWindowHandle = handle.into();
            if let RawWindowHandle::Win32(win) = raw {
                let hwnd = win.hwnd.get() as windows_sys::Win32::Foundation::HWND;
                let flag = if topmost {
                    HWND_TOPMOST
                } else {
                    HWND_NOTOPMOST
                };
                let ok = unsafe { SetWindowPos(hwnd, flag, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE) };
                return ok != 0;
            }
        }
        false
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Not implemented on non-Windows (no-op)
        let _ = (frame, topmost);
        false
    }
}

/// Minimize or restore the window (Windows only).
fn set_window_minimized(frame: &eframe::Frame, minimized: bool) -> bool {
    #[cfg(target_os = "windows")]
    {
        use raw_window_handle::HasWindowHandle;
        use raw_window_handle::RawWindowHandle;
        use windows_sys::Win32::UI::WindowsAndMessaging::{SW_MINIMIZE, SW_RESTORE, ShowWindow};

        // Use the new HasWindowHandle API
        if let Ok(handle) = frame.window_handle() {
            let raw: raw_window_handle::RawWindowHandle = handle.into();
            if let RawWindowHandle::Win32(win) = raw {
                let hwnd = win.hwnd.get() as windows_sys::Win32::Foundation::HWND;
                let cmd = if minimized { SW_MINIMIZE } else { SW_RESTORE };
                let ok = unsafe { ShowWindow(hwnd, cmd) };
                return ok != 0;
            }
        }
        false
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Not implemented on non-Windows (no-op)
        let _ = (frame, minimized);
        false
    }
}

#[cfg(not(target_os = "windows"))]
fn get_window_position(_frame: &eframe::Frame) -> Option<(i32, i32)> {
    None
}

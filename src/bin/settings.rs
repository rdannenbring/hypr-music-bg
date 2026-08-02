//! Settings window for hypr-music-bg.
//!
//! A separate binary behind `--features gui`, so the always-running daemon does
//! not link a UI toolkit it never uses. It talks to the daemon over the same
//! control socket the CLI uses, and edits the config file through
//! `ConfigDocument`, which preserves comments — the shipped example has 128 of
//! them, and they carry most of the explanation of what the settings mean.
//!
//! Nothing here writes to the daemon's memory directly. Changes go to the config
//! file and then a `reload` is sent, so the file on disk stays the source of
//! truth and the GUI can never leave the two disagreeing.

use eframe::egui;
use hypr_music_bg::config_edit::ConfigDocument;
use hypr_music_bg::control::{self, Command, Status};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Every source the daemon knows how to build, for the "add" menu.
const KNOWN_SOURCES: [&str; 10] = [
    "mpris",
    "local",
    "coverartarchive",
    "deezer",
    "itunes",
    "spotify",
    "discogs",
    "fanarttv",
    "subsonic",
    "exec",
];

const RENDER_STYLES: [&str; 3] = ["blur", "fill", "fit"];
const LAYOUTS: [&str; 2] = ["per_monitor", "span"];
const BACKENDS: [&str; 6] = ["auto", "dms", "swww", "hyprpaper", "swaybg", "command"];
const THEME_MODES: [&str; 5] = ["off", "auto", "matugen", "pywal", "command"];
const THEME_SOURCES: [&str; 2] = ["cover", "wallpaper"];

/// What the background thread knows about the daemon.
#[derive(Default)]
struct DaemonState {
    status: Option<Status>,
    /// None until the first poll completes.
    reachable: Option<bool>,
    last_message: Option<String>,
}

enum Request {
    Reload,
    Refresh,
    Restore,
}

fn main() -> eframe::Result<()> {
    let path = config_path();
    let doc = match ConfigDocument::load(&path) {
        Ok(doc) => doc,
        Err(e) => {
            eprintln!("could not read {}: {e:#}", path.display());
            std::process::exit(1);
        }
    };

    let shared = Arc::new(Mutex::new(DaemonState::default()));
    let (tx, rx) = std::sync::mpsc::channel::<Request>();
    spawn_daemon_link(shared.clone(), rx);

    let app = SettingsApp {
        path,
        doc,
        shared,
        tx,
        toast: None,
        add_source: KNOWN_SOURCES[0].to_string(),
    };

    eframe::run_native(
        "hypr-music-bg settings",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([820.0, 660.0])
                .with_min_inner_size([600.0, 440.0])
                .with_app_id("hypr-music-bg-settings"),
            ..Default::default()
        },
        Box::new(|_cc| Ok(Box::new(app))),
    )
}

fn config_path() -> PathBuf {
    // Same resolution the daemon uses, so both edit one file.
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".config")
        });
    base.join("hypr-music-bg").join("config.toml")
}

/// Poll the daemon on a background thread.
///
/// egui redraws on a synchronous loop and the control socket is async, so the
/// two are kept apart: this owns a small runtime, publishes into a mutex, and
/// the UI only ever reads a snapshot. Blocking the UI thread on a socket read
/// would stutter the window whenever the daemon was busy resolving artwork.
fn spawn_daemon_link(shared: Arc<Mutex<DaemonState>>, rx: std::sync::mpsc::Receiver<Request>) {
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("could not start runtime: {e}");
                return;
            }
        };

        loop {
            // Drain queued actions first so a click feels immediate.
            while let Ok(request) = rx.try_recv() {
                let command = match request {
                    Request::Reload => Command::Reload,
                    Request::Refresh => Command::Refresh {
                        bypass_cache: false,
                    },
                    Request::Restore => Command::Restore,
                };
                let reply = runtime.block_on(control::send(&command));
                if let Ok(mut state) = shared.lock() {
                    state.last_message = Some(match reply {
                        Ok(r) => r.message.unwrap_or_else(|| "done".into()),
                        Err(e) => format!("{e}"),
                    });
                }
            }

            match runtime.block_on(control::send(&Command::Status)) {
                Ok(response) => {
                    if let Ok(mut state) = shared.lock() {
                        state.reachable = Some(true);
                        state.status = response.status;
                    }
                }
                Err(_) => {
                    if let Ok(mut state) = shared.lock() {
                        state.reachable = Some(false);
                        state.status = None;
                    }
                }
            }

            std::thread::sleep(Duration::from_millis(900));
        }
    });
}

struct SettingsApp {
    path: PathBuf,
    doc: ConfigDocument,
    shared: Arc<Mutex<DaemonState>>,
    tx: std::sync::mpsc::Sender<Request>,
    toast: Option<(String, Instant)>,
    add_source: String,
}

impl SettingsApp {
    fn note(&mut self, message: impl Into<String>) {
        self.toast = Some((message.into(), Instant::now()));
    }

    /// Write the config, then ask the daemon to re-read it.
    fn save(&mut self) {
        match self.doc.save(&self.path) {
            Ok(true) => {
                self.tx.send(Request::Reload).ok();
                self.note("Saved and reloaded");
            }
            Ok(false) => self.note("No changes to save"),
            Err(e) => self.note(format!("Save failed: {e:#}")),
        }
    }

    fn int_row(&mut self, ui: &mut egui::Ui, label: &str, key: &str, default: i64, hint: &str) {
        let mut value = self.doc.get_i64(key).unwrap_or(default);
        ui.horizontal(|ui| {
            ui.label(label);
            if ui
                .add(egui::DragValue::new(&mut value).range(0..=100_000))
                .changed()
            {
                self.doc.set(key, value);
            }
            ui.label(egui::RichText::new(hint).weak().small());
        });
    }

    fn float_row(
        &mut self,
        ui: &mut egui::Ui,
        label: &str,
        key: &str,
        default: f64,
        range: std::ops::RangeInclusive<f64>,
    ) {
        let mut value = self.doc.get_f64(key).unwrap_or(default);
        ui.horizontal(|ui| {
            ui.label(label);
            if ui
                .add(egui::Slider::new(&mut value, range).fixed_decimals(2))
                .changed()
            {
                self.doc.set(key, value);
            }
        });
    }

    fn bool_row(&mut self, ui: &mut egui::Ui, label: &str, key: &str, default: bool, hint: &str) {
        let mut value = self.doc.get_bool(key).unwrap_or(default);
        if ui.checkbox(&mut value, label).changed() {
            self.doc.set(key, value);
        }
        if !hint.is_empty() {
            ui.label(egui::RichText::new(hint).weak().small());
        }
    }

    fn text_row(&mut self, ui: &mut egui::Ui, label: &str, key: &str, hint: &str) {
        let mut value = self.doc.get_str(key).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label(label);
            if ui
                .add(egui::TextEdit::singleline(&mut value).hint_text(hint))
                .changed()
            {
                // An emptied field means "unset", not "set to empty string" —
                // otherwise clearing a path would leave a "" the daemon has to
                // special-case.
                if value.trim().is_empty() {
                    self.doc.remove(key);
                } else {
                    self.doc.set(key, value.as_str());
                }
            }
        });
    }

    fn choice_row(&mut self, ui: &mut egui::Ui, label: &str, key: &str, options: &[&str]) {
        let current = self
            .doc
            .get_str(key)
            .unwrap_or_else(|| options[0].to_string());
        ui.horizontal(|ui| {
            ui.label(label);
            egui::ComboBox::from_id_salt(key)
                .selected_text(&current)
                .show_ui(ui, |ui| {
                    for option in options {
                        if ui.selectable_label(current == *option, *option).clicked() {
                            self.doc.set(key, *option);
                        }
                    }
                });
        });
    }

    fn list_row(&mut self, ui: &mut egui::Ui, label: &str, key: &str, hint: &str) {
        let current = self.doc.get_string_array(key).unwrap_or_default();
        let mut text = current.join(", ");
        ui.horizontal(|ui| {
            ui.label(label);
            if ui
                .add(egui::TextEdit::singleline(&mut text).hint_text(hint))
                .changed()
            {
                let values: Vec<String> = text
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                self.doc.set_string_array(key, &values);
            }
        });
    }

    fn sources_panel(&mut self, ui: &mut egui::Ui) {
        ui.label(
            egui::RichText::new(
                "Tried in order. The first that both matches the track and meets the \
                 resolution floor wins.",
            )
            .weak()
            .small(),
        );
        ui.add_space(4.0);

        let names = self.doc.source_names();
        if names.is_empty() {
            ui.label("No sources configured — the daemon will use its defaults.");
        }

        // Collected rather than applied inside the loop: mutating the document
        // while iterating its own contents would invalidate the indices.
        let mut swap: Option<(usize, usize)> = None;
        let mut remove: Option<usize> = None;

        for (index, name) in names.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("{}.", index + 1));
                ui.label(egui::RichText::new(name).strong());

                if ui
                    .add_enabled(index > 0, egui::Button::new("↑"))
                    .on_hover_text("Move earlier in the chain")
                    .clicked()
                {
                    swap = Some((index, index - 1));
                }
                if ui
                    .add_enabled(index + 1 < names.len(), egui::Button::new("↓"))
                    .on_hover_text("Move later in the chain")
                    .clicked()
                {
                    swap = Some((index, index + 1));
                }
                if ui.button("Remove").clicked() {
                    remove = Some(index);
                }

                if matches!(
                    name.as_str(),
                    "coverartarchive" | "deezer" | "itunes" | "subsonic"
                ) {
                    let mut size = self.doc.source_key_i64(index, "size").unwrap_or(1200);
                    ui.label("size");
                    if ui
                        .add(egui::DragValue::new(&mut size).range(0..=4000))
                        .on_hover_text("0 on coverartarchive fetches the original upload")
                        .changed()
                    {
                        self.doc.set_source_key(index, "size", size);
                    }
                }
            });
        }

        if let Some((a, b)) = swap {
            self.doc.swap_sources(a, b);
        }
        if let Some(index) = remove {
            self.doc.remove_source(index);
        }

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("add_source")
                .selected_text(&self.add_source)
                .show_ui(ui, |ui| {
                    for option in KNOWN_SOURCES {
                        ui.selectable_value(&mut self.add_source, option.to_string(), option);
                    }
                });
            if ui.button("Add source").clicked() {
                let name = self.add_source.clone();
                self.doc.add_source(&name);
            }
        });
        ui.label(
            egui::RichText::new(
                "Credential-backed sources (spotify, discogs, fanarttv, subsonic) need their \
                 env-var keys added in the config file. They are referenced by variable name, \
                 never stored here.",
            )
            .weak()
            .small(),
        );
    }

    fn status_panel(&self, ui: &mut egui::Ui) {
        let Ok(state) = self.shared.lock() else {
            return;
        };

        match state.reachable {
            None => {
                ui.label("Connecting…");
                return;
            }
            Some(false) => {
                ui.colored_label(egui::Color32::from_rgb(200, 120, 60), "Daemon not running");
                ui.label(
                    egui::RichText::new("Changes are still saved to the config file.")
                        .weak()
                        .small(),
                );
                return;
            }
            Some(true) => {}
        }

        let Some(status) = &state.status else {
            return;
        };

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Daemon").strong());
            if status.enabled {
                ui.colored_label(egui::Color32::from_rgb(110, 180, 110), "enabled");
            } else {
                ui.colored_label(egui::Color32::from_rgb(200, 120, 60), "disabled");
            }
            ui.label(&status.playback);
        });

        ui.label(format!(
            "{} — {}",
            status.artist.as_deref().unwrap_or("?"),
            status.album.as_deref().unwrap_or("?")
        ));
        if let Some(title) = &status.title {
            ui.label(egui::RichText::new(title).weak());
        }

        ui.add_space(4.0);
        match &status.art {
            Some(art) => {
                ui.label(format!(
                    "{} · {}×{} · {} KiB",
                    art.source,
                    art.width,
                    art.height,
                    art.bytes / 1024
                ));
                if art.degraded {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 150, 60),
                        format!("below the {}px floor", status.min_resolution),
                    );
                }
            }
            None => {
                ui.label(egui::RichText::new("no artwork resolved").weak());
            }
        }

        if !status.chain.is_empty() {
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Last resolve").strong());
            for outcome in &status.chain {
                ui.label(
                    egui::RichText::new(format!("{}: {}", outcome.source, outcome.outcome)).small(),
                );
            }
        }

        ui.add_space(6.0);
        ui.separator();
        ui.label(
            egui::RichText::new(format!("build {}", status.build.summary()))
                .weak()
                .small(),
        );
        ui.label(
            egui::RichText::new(format!(
                "backend {} · theming {} · cache {} MiB",
                status.backend,
                status.theme_mode,
                status.cache_bytes / 1_048_576
            ))
            .weak()
            .small(),
        );
    }
}

impl eframe::App for SettingsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // The status panel is polled from a background thread, so repaint on a
        // timer rather than only on input.
        ui.ctx().request_repaint_after(Duration::from_millis(500));

        // Panels nest outermost-first, and the central panel must come last.
        egui::containers::Panel::bottom("actions").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let dirty = self.doc.is_modified();
                if ui
                    .add_enabled(dirty, egui::Button::new("Save and reload"))
                    .clicked()
                {
                    self.save();
                }
                if ui.button("Refresh artwork").clicked() {
                    self.tx.send(Request::Refresh).ok();
                    self.note("Refreshing…");
                }
                if ui.button("Restore wallpaper").clicked() {
                    self.tx.send(Request::Restore).ok();
                    self.note("Restoring…");
                }
                if dirty {
                    ui.colored_label(egui::Color32::from_rgb(200, 150, 60), "unsaved changes");
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Toasts fade rather than accumulating.
                    let expired = self
                        .toast
                        .as_ref()
                        .is_some_and(|(_, at)| at.elapsed() >= Duration::from_secs(6));
                    if expired {
                        self.toast = None;
                    }
                    if let Some((message, _)) = &self.toast {
                        ui.label(egui::RichText::new(message).small());
                    } else if let Ok(state) = self.shared.lock()
                        && let Some(message) = &state.last_message
                    {
                        ui.label(egui::RichText::new(message).weak().small());
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::containers::Panel::right("status")
            .resizable(true)
            .show(ui, |ui| {
                ui.set_min_width(280.0);
                ui.add_space(6.0);
                self.status_panel(ui);
            });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.label(
                egui::RichText::new(self.path.display().to_string())
                    .weak()
                    .small(),
            );
            ui.add_space(4.0);

            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::CollapsingHeader::new("Source chain")
                    .default_open(true)
                    .show(ui, |ui| self.sources_panel(ui));

                egui::CollapsingHeader::new("Artwork")
                    .default_open(true)
                    .show(ui, |ui| {
                        self.int_row(
                            ui,
                            "Minimum resolution",
                            "art.min_resolution",
                            600,
                            "px on the shorter edge",
                        );
                        self.bool_row(
                            ui,
                            "Verify candidates match the track",
                            "art.verify_match",
                            true,
                            "leave on: some catalogues answer with the wrong album",
                        );
                        self.float_row(
                            ui,
                            "Match threshold",
                            "art.match_threshold",
                            0.6,
                            0.0..=1.0,
                        );
                        self.bool_row(
                            ui,
                            "Allow degraded art",
                            "art.allow_degraded",
                            true,
                            "use the largest found when nothing meets the floor",
                        );
                        self.text_row(
                            ui,
                            "Fallback wallpaper",
                            "art.fallback_wallpaper",
                            "file or directory",
                        );
                    });

                egui::CollapsingHeader::new("Cache").show(ui, |ui| {
                    self.int_row(
                        ui,
                        "Size budget",
                        "art.cache_max_mb",
                        512,
                        "MiB, 0 disables",
                    );
                    self.int_row(
                        ui,
                        "Maximum age",
                        "art.cache_max_age_days",
                        30,
                        "days, 0 disables",
                    );
                    self.int_row(
                        ui,
                        "Negative cache TTL",
                        "art.negative_cache_ttl",
                        86_400,
                        "seconds to remember an album has no art",
                    );
                });

                egui::CollapsingHeader::new("Render").show(ui, |ui| {
                    self.choice_row(ui, "Style", "render.style", &RENDER_STYLES);
                    self.choice_row(ui, "Layout", "render.layout", &LAYOUTS);
                    self.float_row(ui, "Cover scale", "render.cover_scale", 0.55, 0.05..=1.0);
                    self.float_row(ui, "Blur strength", "render.blur_strength", 8.0, 0.0..=30.0);
                    self.float_row(ui, "Darken", "render.darken", 0.25, 0.0..=1.0);
                });

                egui::CollapsingHeader::new("Wallpaper backend").show(ui, |ui| {
                    self.choice_row(ui, "Backend", "wallpaper.backend", &BACKENDS);
                    self.text_row(
                        ui,
                        "Command",
                        "wallpaper.command",
                        "for backend = command; {image} and {monitor}",
                    );
                });

                egui::CollapsingHeader::new("Colour theming").show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(
                            "Off by default. Recolours GTK, the terminal and the browser on \
                             every album change. 'auto' skips shells that already theme \
                             themselves from the wallpaper.",
                        )
                        .weak()
                        .small(),
                    );
                    self.choice_row(ui, "Mode", "theme.mode", &THEME_MODES);
                    self.choice_row(ui, "Sample", "theme.source", &THEME_SOURCES);
                    self.text_row(
                        ui,
                        "Command",
                        "theme.command",
                        "for mode = command; {image}",
                    );
                });

                egui::CollapsingHeader::new("Players").show(ui, |ui| {
                    self.list_row(ui, "Prefer", "player.prefer", "comma separated");
                    self.list_row(
                        ui,
                        "Ignore",
                        "player.ignore",
                        "browsers and proxies, comma separated",
                    );
                });

                egui::CollapsingHeader::new("Behaviour").show(ui, |ui| {
                    self.bool_row(
                        ui,
                        "Restore wallpaper on pause",
                        "behavior.restore_on_pause",
                        false,
                        "",
                    );
                    self.bool_row(
                        ui,
                        "Restore wallpaper on stop",
                        "behavior.restore_on_stop",
                        true,
                        "",
                    );
                });
            });
        });
    }
}

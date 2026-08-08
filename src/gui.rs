use crate::state::SharedState;
use crate::{audio, network, settings};
use eframe::egui;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub struct VoipApp {
    state: SharedState,
    username: String,

    input_devices: Vec<String>,
    output_devices: Vec<String>,
    selected_input: String,
    selected_output: String,

    connected: bool,
    voice: Option<audio::VoiceHandles>,
    discovery_handles: Vec<JoinHandle<()>>,

    last_cleanup: Instant,
}

impl VoipApp {
    pub fn new() -> Self {
        let input_devices = audio::list_input_devices();
        let output_devices = audio::list_output_devices();
        let selected_input = audio::default_input_device_name()
            .or_else(|| input_devices.first().cloned())
            .unwrap_or_default();
        let selected_output = audio::default_output_device_name()
            .or_else(|| output_devices.first().cloned())
            .unwrap_or_default();

        Self {
            state: SharedState::new(),
            username: settings::generate_random_username(),
            input_devices,
            output_devices,
            selected_input,
            selected_output,
            connected: false,
            voice: None,
            discovery_handles: Vec::new(),
            last_cleanup: Instant::now(),
        }
    }

    fn connect(&mut self) {
        if self.connected {
            return;
        }

        let username = if self.username.trim().is_empty() {
            settings::generate_random_username()
        } else {
            self.username.trim().to_string()
        };
        self.username = username.clone();

        self.state.running.store(true, Ordering::Relaxed);
        let discovery_handles = network::start_discovery(
            username,
            settings::DEFAULT_BROADCAST_ADDR.to_string(),
            self.state.clone(),
        );

        match audio::start_voice(self.state.clone(), &self.selected_input, &self.selected_output) {
            Ok(handles) => {
                self.discovery_handles = discovery_handles;
                self.voice = Some(handles);
                self.connected = true;
                self.state.log("Подключено");
            }
            Err(e) => {
                self.state.log(format!("Не удалось запустить звук: {e}"));
                // Discovery уже запустили — аккуратно останавливаем его,
                // раз аудио не поднялось, чтобы не остаться в подвешенном состоянии.
                self.state.running.store(false, Ordering::Relaxed);
                for h in discovery_handles {
                    let _ = h.join();
                }
            }
        }
    }

    fn disconnect(&mut self) {
        if !self.connected {
            return;
        }
        self.state.running.store(false, Ordering::Relaxed);
        if let Some(voice) = self.voice.take() {
            voice.stop();
        }
        for h in std::mem::take(&mut self.discovery_handles) {
            let _ = h.join();
        }
        self.state.peers.lock().unwrap().clear();
        self.connected = false;
        self.state.log("Отключено");
    }

    /// Перезапускает только звук (например, при смене устройства ввода/вывода),
    /// не трогая discovery — остальные участники не "отваливаются" на это время.
    fn restart_voice(&mut self) {
        if !self.connected {
            return;
        }
        if let Some(voice) = self.voice.take() {
            voice.stop();
        }
        match audio::start_voice(self.state.clone(), &self.selected_input, &self.selected_output) {
            Ok(handles) => {
                self.voice = Some(handles);
                self.state.log("Аудио-устройство переключено");
            }
            Err(e) => {
                self.state.log(format!("Не удалось переключить устройство: {e}"));
            }
        }
    }
}

impl eframe::App for VoipApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Периодически чистим "отвалившихся" участников. Экран перерисовываем
        // регулярно сами — иначе изменения из фоновых потоков (новые пиры,
        // строки в логах) не появятся на экране без действий пользователя.
        if self.connected && self.last_cleanup.elapsed() > Duration::from_secs(1) {
            network::cleanup_stale_peers(&self.state, Duration::from_secs(settings::PEER_TIMEOUT_SECS));
            self.last_cleanup = Instant::now();
        }
        ctx.request_repaint_after(Duration::from_millis(200));

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(8.0);

            // --- Имя пользователя ---
            ui.add_enabled(
                !self.connected,
                egui::TextEdit::singleline(&mut self.username)
                    .hint_text("Имя")
                    .desired_width(f32::INFINITY),
            );

            ui.add_space(12.0);

            // --- Громкость каждого собеседника ---
            let peers: Vec<(std::net::SocketAddr, String)> = {
                let map = self.state.peers.lock().unwrap();
                let mut v: Vec<_> = map.iter().map(|(addr, p)| (*addr, p.username.clone())).collect();
                v.sort_by(|a, b| a.1.cmp(&b.1));
                v
            };

            if peers.is_empty() {
                ui.weak("Участников пока нет");
            } else {
                for (addr, name) in &peers {
                    let mut gain = self.state.peer_gain(addr);
                    ui.horizontal(|ui| {
                        ui.label(name);
                        if ui.add(egui::Slider::new(&mut gain, 0.0..=2.0)).changed() {
                            self.state.set_peer_gain(*addr, gain);
                        }
                    });
                }
            }

            ui.add_space(10.0);

            // --- Микрофон: иконка-мьют + слайдер громкости в одной строке ---
            ui.horizontal(|ui| {
                let mic_muted = self.state.mic_muted.load(Ordering::Relaxed);
                if mic_icon_button(ui, mic_muted).clicked() {
                    self.state.mic_muted.store(!mic_muted, Ordering::Relaxed);
                }

                let mut gain = *self.state.mic_gain.lock().unwrap();
                if ui.add(egui::Slider::new(&mut gain, 0.0..=2.0)).changed() {
                    *self.state.mic_gain.lock().unwrap() = gain;
                }
            });

            ui.add_space(8.0);

            // --- Весь входящий звук: иконка-мьют ---
            ui.horizontal(|ui| {
                let sound_muted = self.state.sound_muted.load(Ordering::Relaxed);
                if sound_icon_button(ui, sound_muted).clicked() {
                    self.state.sound_muted.store(!sound_muted, Ordering::Relaxed);
                }
                ui.label("Весь звук");
            });

            ui.add_space(8.0);

            // --- Выбор устройств ввода/вывода (клик открывает выпадающий список) ---
            let mut device_changed = false;
            ui.horizontal(|ui| {
                let half_width = ui.available_width() / 2.0 - 4.0;

                ui.scope(|ui| {
                    ui.set_width(half_width);
                    egui::ComboBox::from_id_source("input_device")
                        .width(half_width)
                        .selected_text(&self.selected_input)
                        .show_ui(ui, |ui| {
                            for name in self.input_devices.clone() {
                                if ui.selectable_value(&mut self.selected_input, name.clone(), &name).changed() {
                                    device_changed = true;
                                }
                            }
                        });
                });

                egui::ComboBox::from_id_source("output_device")
                    .width(ui.available_width())
                    .selected_text(&self.selected_output)
                    .show_ui(ui, |ui| {
                        for name in self.output_devices.clone() {
                            if ui.selectable_value(&mut self.selected_output, name.clone(), &name).changed() {
                                device_changed = true;
                            }
                        }
                    });
            });
            if device_changed {
                self.restart_voice();
            }

            ui.add_space(16.0);

            // --- Подключиться / Отключиться ---
            let button_text = if self.connected { "Отключиться" } else { "Подключиться" };
            let button_color = if self.connected {
                egui::Color32::from_rgb(220, 53, 53)
            } else {
                egui::Color32::from_rgb(76, 175, 80)
            };
            if ui
                .add_sized([ui.available_width(), 40.0], egui::Button::new(button_text).fill(button_color))
                .clicked()
            {
                if self.connected {
                    self.disconnect();
                } else {
                    self.connect();
                }
            }

            ui.add_space(10.0);

            // --- Логи: свёрнуты по умолчанию, разворачиваются по клику ---
            egui::CollapsingHeader::new("Логи")
                .default_open(false)
                .show(ui, |ui| {
                    egui::Frame::none()
                        .fill(egui::Color32::from_gray(60))
                        .inner_margin(6.0)
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .stick_to_bottom(true)
                                .max_height(160.0)
                                .show(ui, |ui| {
                                    let logs = self.state.logs.lock().unwrap();
                                    for line in logs.iter() {
                                        ui.colored_label(egui::Color32::WHITE, line);
                                    }
                                });
                        });
                });
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Аккуратно останавливаем потоки при закрытии окна, чтобы порты
        // освободились сразу, а не спустя таймаут ОС.
        self.disconnect();
    }
}

/// Кнопка-иконка микрофона. Рисуется вручную через Painter (капсула-головка +
/// стойка + основание), а не эмодзи-символом — так внешний вид не зависит
/// от того, есть ли в системном/встроенном шрифте нужный эмодзи-глиф.
/// Цвет фона показывает состояние: зелёный — включён, красный — замьючен
/// (плюс диагональная черта поверх иконки для дополнительной ясности).
fn mic_icon_button(ui: &mut egui::Ui, muted: bool) -> egui::Response {
    let size = egui::vec2(44.0, 32.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());

    if ui.is_rect_visible(rect) {
        let bg = if muted {
            egui::Color32::from_rgb(150, 40, 40)
        } else {
            egui::Color32::from_rgb(76, 175, 80)
        };
        let painter = ui.painter();
        painter.rect_filled(rect, egui::Rounding::same(6.0), bg);

        let icon_color = egui::Color32::WHITE;
        let c = rect.center() - egui::vec2(0.0, 2.0);

        // Головка микрофона
        let head_rect = egui::Rect::from_center_size(c, egui::vec2(9.0, 13.0));
        painter.rect_filled(head_rect, egui::Rounding::same(4.5), icon_color);

        // Стойка вниз от головки
        let stand_bottom = c + egui::vec2(0.0, 11.0);
        painter.line_segment([head_rect.center_bottom(), stand_bottom], egui::Stroke::new(2.0, icon_color));
        // Основание (подставка)
        painter.line_segment(
            [stand_bottom - egui::vec2(5.0, 0.0), stand_bottom + egui::vec2(5.0, 0.0)],
            egui::Stroke::new(2.0, icon_color),
        );

        if muted {
            painter.line_segment(
                [rect.left_top() + egui::vec2(6.0, 6.0), rect.right_bottom() - egui::vec2(6.0, 6.0)],
                egui::Stroke::new(2.5, icon_color),
            );
        }
    }

    response.on_hover_text(if muted { "Микрофон выключен" } else { "Микрофон включён" })
}

/// Кнопка-иконка динамика (весь входящий звук). Тот же подход: корпус +
/// раструб рисуются вручную, а не эмодзи, цвет фона показывает состояние.
fn sound_icon_button(ui: &mut egui::Ui, muted: bool) -> egui::Response {
    let size = egui::vec2(44.0, 32.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());

    if ui.is_rect_visible(rect) {
        let bg = if muted {
            egui::Color32::from_rgb(150, 40, 40)
        } else {
            egui::Color32::from_rgb(76, 175, 80)
        };
        let painter = ui.painter();
        painter.rect_filled(rect, egui::Rounding::same(6.0), bg);

        let icon_color = egui::Color32::WHITE;
        let c = rect.center();

        // Корпус динамика (прямоугольник)
        let box_rect = egui::Rect::from_min_size(egui::pos2(c.x - 10.0, c.y - 5.0), egui::vec2(6.0, 10.0));
        painter.rect_filled(box_rect, egui::Rounding::ZERO, icon_color);

        // Раструб (треугольная "юбка" колонки)
        let cone_points = vec![
            egui::pos2(box_rect.right(), box_rect.top()),
            egui::pos2(box_rect.right(), box_rect.bottom()),
            egui::pos2(box_rect.right() + 9.0, box_rect.center().y + 6.0),
            egui::pos2(box_rect.right() + 9.0, box_rect.center().y - 6.0),
        ];
        painter.add(egui::Shape::convex_polygon(cone_points, icon_color, egui::Stroke::NONE));

        if muted {
            painter.line_segment(
                [rect.left_top() + egui::vec2(6.0, 6.0), rect.right_bottom() - egui::vec2(6.0, 6.0)],
                egui::Stroke::new(2.5, icon_color),
            );
        }
    }

    response.on_hover_text(if muted { "Звук выключен" } else { "Звук включён" })
}

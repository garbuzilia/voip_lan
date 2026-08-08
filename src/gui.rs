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

            // --- Мьют микрофона + громкость + мьют звука — всё в одну строку ---
            ui.horizontal(|ui| {
                let mic_muted = self.state.mic_muted.load(Ordering::Relaxed);
                if mic_icon_button(ui, mic_muted).clicked() {
                    self.state.mic_muted.store(!mic_muted, Ordering::Relaxed);
                }

                let sound_muted = self.state.sound_muted.load(Ordering::Relaxed);
                let slider_width = ui.available_width() - 40.0 - 8.0; // минус кнопка звука и отступ

                let mut gain = *self.state.mic_gain.lock().unwrap();
                if ui
                    .add_sized(
                        [slider_width.max(40.0), 32.0],
                        egui::Slider::new(&mut gain, 0.0..=2.0),
                    )
                    .changed()
                {
                    *self.state.mic_gain.lock().unwrap() = gain;
                }

                if sound_icon_button(ui, sound_muted).clicked() {
                    self.state.sound_muted.store(!sound_muted, Ordering::Relaxed);
                }
            });

            ui.add_space(10.0);

            // --- Выбор устройств: каждое на своей строке во всю ширину,
            //     название обрезается, чтобы никогда не вылезти за окно ---
            let window_width = ui.available_width();
            let mut device_changed = false;

            ui.label("Микрофон:");
            egui::ComboBox::from_id_source("input_device")
                .width(window_width)
                .selected_text(truncate_label(&self.selected_input, 40))
                .show_ui(ui, |ui| {
                    for name in self.input_devices.clone() {
                        if ui
                            .selectable_value(&mut self.selected_input, name.clone(), truncate_label(&name, 60))
                            .changed()
                        {
                            device_changed = true;
                        }
                    }
                });

            ui.add_space(6.0);

            ui.label("Наушники / динамики:");
            egui::ComboBox::from_id_source("output_device")
                .width(window_width)
                .selected_text(truncate_label(&self.selected_output, 40))
                .show_ui(ui, |ui| {
                    for name in self.output_devices.clone() {
                        if ui
                            .selectable_value(&mut self.selected_output, name.clone(), truncate_label(&name, 60))
                            .changed()
                        {
                            device_changed = true;
                        }
                    }
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
            let full_width = ui.available_width();
            egui::CollapsingHeader::new("Логи")
                .default_open(false)
                .show(ui, |ui| {
                    ui.set_min_width(full_width);
                    egui::Frame::none()
                        .fill(egui::Color32::from_gray(60))
                        .inner_margin(6.0)
                        .show(ui, |ui| {
                            ui.set_min_width(full_width - 12.0);
                            egui::ScrollArea::vertical()
                                .stick_to_bottom(true)
                                .max_height(160.0)
                                .show(ui, |ui| {
                                    ui.set_min_width(full_width - 24.0);
                                    ui.set_min_height(140.0);
                                    let logs = self.state.logs.lock().unwrap();
                                    if logs.is_empty() {
                                        ui.weak("Пока пусто");
                                    } else {
                                        for line in logs.iter() {
                                            ui.colored_label(egui::Color32::WHITE, line);
                                        }
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

/// Обрезает длинное название устройства, чтобы оно гарантированно
/// помещалось в фиксированную ширину окна (в egui ComboBox сам текст
/// не переносится и не обрезается автоматически).
fn truncate_label(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

/// Кнопка-иконка микрофона. Используются только простые, надёжно
/// поддерживаемые примитивы (прямоугольники, линии) — без самодельных
/// многоугольников, чтобы не ловить проблемы с порядком точек/заливкой.
/// Цвет фона показывает состояние: зелёный — включён, красный — замьючен
/// (плюс диагональная черта поверх иконки для дополнительной ясности).
fn mic_icon_button(ui: &mut egui::Ui, muted: bool) -> egui::Response {
    let size = egui::vec2(40.0, 32.0);
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
        let c = rect.center() - egui::vec2(0.0, 3.0);

        // Головка микрофона — капсула (закруглённый прямоугольник)
        let head_w = 10.0;
        let head_h = 14.0;
        let head_rect = egui::Rect::from_center_size(c, egui::vec2(head_w, head_h));
        painter.rect_filled(head_rect, egui::Rounding::same(head_w / 2.0), icon_color);

        // Две тонкие полоски-"решётка", чтобы форма явно читалась как микрофон
        painter.line_segment(
            [head_rect.left_center() + egui::vec2(0.0, -2.0), head_rect.right_center() + egui::vec2(0.0, -2.0)],
            egui::Stroke::new(1.0, bg),
        );
        painter.line_segment(
            [head_rect.left_center() + egui::vec2(0.0, 2.0), head_rect.right_center() + egui::vec2(0.0, 2.0)],
            egui::Stroke::new(1.0, bg),
        );

        // Стойка и основание под микрофоном
        let stand_bottom = head_rect.center_bottom() + egui::vec2(0.0, 8.0);
        painter.line_segment([head_rect.center_bottom(), stand_bottom], egui::Stroke::new(2.0, icon_color));
        painter.line_segment(
            [stand_bottom - egui::vec2(5.0, 0.0), stand_bottom + egui::vec2(5.0, 0.0)],
            egui::Stroke::new(2.0, icon_color),
        );

        if muted {
            painter.line_segment(
                [rect.left_top() + egui::vec2(5.0, 5.0), rect.right_bottom() - egui::vec2(5.0, 5.0)],
                egui::Stroke::new(2.5, icon_color),
            );
        }
    }

    response.on_hover_text(if muted { "Микрофон выключен (нажмите, чтобы включить)" } else { "Микрофон включён (нажмите, чтобы выключить)" })
}

/// Кнопка-иконка динамика (весь входящий звук). Корпус — прямоугольник,
/// раструб — круг сбоку (вместо треугольника-полигона, который отрисовался
/// некорректно). Такое сочетание тоже читается как "динамик/громкость",
/// но использует только простые примитивы.
fn sound_icon_button(ui: &mut egui::Ui, muted: bool) -> egui::Response {
    let size = egui::vec2(40.0, 32.0);
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

        // Корпус динамика
        let box_rect = egui::Rect::from_center_size(c - egui::vec2(5.0, 0.0), egui::vec2(7.0, 12.0));
        painter.rect_filled(box_rect, egui::Rounding::same(1.5), icon_color);

        // Раструб — круг, слегка перекрывающий корпус справа
        painter.circle_filled(box_rect.right_center() + egui::vec2(4.0, 0.0), 7.0, icon_color);

        if muted {
            painter.line_segment(
                [rect.left_top() + egui::vec2(5.0, 5.0), rect.right_bottom() - egui::vec2(5.0, 5.0)],
                egui::Stroke::new(2.5, icon_color),
            );
        }
    }

    response.on_hover_text(if muted { "Звук выключен (нажмите, чтобы включить)" } else { "Звук включён (нажмите, чтобы выключить)" })
}

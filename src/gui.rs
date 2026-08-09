use crate::state::SharedState;
use crate::{audio, network, settings};
use eframe::egui;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const WINDOW_WIDTH: f32 = 380.0;
/// Стартовая высота окна — не обязана быть точной: сразу после первого
/// кадра окно само подгонится под реальный размер содержимого (см. update()),
/// поэтому не нужно вручную высчитывать пиксели под каждый элемент.
pub const INITIAL_HEIGHT: f32 = 460.0;
/// Нижний предел высоты окна: не даём окну сжаться до почти квадратных
/// пропорций даже когда контента совсем мало (пустой список участников).
const MIN_WINDOW_HEIGHT: f32 = 460.0;
/// Верхний предел высоты окна (на случай очень длинного списка участников
/// или большого количества логов) — дальше начинает работать прокрутка,
/// а не бесконечный рост окна.
const MAX_WINDOW_HEIGHT: f32 = 700.0;
/// Зарезервированная минимальная высота под блок участников — так
/// настройки ниже всегда начинаются на одной и той же высоте, даже
/// когда участников мало или нет совсем (тогда просто пустое место).
const PEERS_SECTION_MIN_HEIGHT: f32 = 110.0;

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
    /// Когда в последний раз перечисляли устройства ввода/вывода —
    /// список обновляется периодически, чтобы подключённые "на лету"
    /// микрофон/наушники появлялись без перезапуска программы.
    last_device_refresh: Instant,

    /// Открыта ли панель логов (передаётся в CollapsingHeader и
    /// переключается по клику на заголовок).
    logs_open: bool,
    /// Последний размер окна, который мы сами применили — чтобы не слать
    /// команду на resize каждый кадр, а только когда реальный размер
    /// контента изменился.
    last_window_size: egui::Vec2,
    /// Снимок того, что реально влияет на высоту layout'а (количество
    /// участников и открыт ли лог). Пересчитываем и меняем размер окна
    /// ТОЛЬКО когда это снимок реально поменялся — а не на каждом кадре,
    /// иначе постоянные resize-команды заметно мешают перетаскиванию
    /// окна мышкой (конфликтуют с системным перемещением окна).
    last_layout_signature: (usize, bool),
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
            last_device_refresh: Instant::now(),
            logs_open: false,
            last_window_size: egui::vec2(WINDOW_WIDTH, INITIAL_HEIGHT),
            last_layout_signature: (0, false),
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
        // Отключаем плавную анимацию сворачивания/разворачивания у виджетов
        // (например, у CollapsingHeader с логами). Без этого измеренная
        // высота контента меняется постепенно на протяжении ~12-15 кадров,
        // и автоподгонка размера окна ниже честно следует за каждым
        // промежуточным значением — окно "доезжает" до цели желеобразными
        // рывками вместо одного мгновенного прыжка.
        ctx.style_mut(|style| style.animation_time = 0.0);

        // Периодически чистим "отвалившихся" участников. Экран перерисовываем
        // регулярно сами — иначе изменения из фоновых потоков (новые пиры,
        // строки в логах) не появятся на экране без действий пользователя.
        if self.connected && self.last_cleanup.elapsed() > Duration::from_secs(1) {
            network::cleanup_stale_peers(&self.state, Duration::from_secs(settings::PEER_TIMEOUT_SECS));
            self.last_cleanup = Instant::now();
        }

        // Периодически обновляем список устройств — чтобы подключённые
        // "на лету" микрофон/наушники появились в выпадающем списке
        // без перезапуска программы.
        if self.last_device_refresh.elapsed() > Duration::from_secs(2) {
            self.input_devices = audio::list_input_devices();
            self.output_devices = audio::list_output_devices();
            self.last_device_refresh = Instant::now();
        }
        ctx.request_repaint_after(Duration::from_millis(200));

        let mut peer_count = 0usize;

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .max_height(MAX_WINDOW_HEIGHT)
                .show(ui, |ui| {
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
            peer_count = peers.len();

            ui.scope(|ui| {
                ui.set_min_height(PEERS_SECTION_MIN_HEIGHT);
                if peers.is_empty() {
                    ui.weak("Участников пока нет");
                } else {
                    for (addr, name) in &peers {
                        let mut gain = self.state.peer_gain(addr);
                        ui.horizontal(|ui| {
                            ui.label(name);
                            let remaining = (ui.available_width() - 55.0).max(40.0);
                            ui.spacing_mut().slider_width = remaining;
                            if ui.add(egui::Slider::new(&mut gain, 0.0..=2.0)).changed() {
                                self.state.set_peer_gain(*addr, gain);
                            }
                        });
                    }
                }
            });

            ui.add_space(10.0);

            // --- Мьют микрофона / звука — обычные подписанные кнопки в одну строку ---
            let full_width = ui.available_width();
            let half_width = (full_width - 8.0) / 2.0;
            ui.horizontal(|ui| {
                let mic_muted = self.state.mic_muted.load(Ordering::Relaxed);
                let mic_color = if mic_muted {
                    egui::Color32::from_rgb(150, 40, 40)
                } else {
                    egui::Color32::from_rgb(76, 175, 80)
                };
                if ui
                    .add_sized([half_width, 34.0], egui::Button::new("Микрофон").fill(mic_color))
                    .clicked()
                {
                    self.state.mic_muted.store(!mic_muted, Ordering::Relaxed);
                }

                let sound_muted = self.state.sound_muted.load(Ordering::Relaxed);
                let sound_color = if sound_muted {
                    egui::Color32::from_rgb(150, 40, 40)
                } else {
                    egui::Color32::from_rgb(76, 175, 80)
                };
                if ui
                    .add_sized([half_width, 34.0], egui::Button::new("Звук").fill(sound_color))
                    .clicked()
                {
                    self.state.sound_muted.store(!sound_muted, Ordering::Relaxed);
                }
            });

            ui.add_space(10.0);

            // --- Громкость микрофона: подписанный слайдер на всю ширину ---
            ui.label("Громкость микрофона");
            {
                let mut gain = *self.state.mic_gain.lock().unwrap();
                ui.spacing_mut().slider_width = full_width - 55.0;
                if ui.add(egui::Slider::new(&mut gain, 0.0..=2.0)).changed() {
                    *self.state.mic_gain.lock().unwrap() = gain;
                }
            }

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

            // --- Логи: свёрнуты по умолчанию; при разворачивании окно
            //     программы увеличивается по высоте (см. update()) ---
            let full_width = ui.available_width();
            let header_response = egui::CollapsingHeader::new("Логи")
                .open(Some(self.logs_open))
                .show(ui, |ui| {
                    ui.set_min_width(full_width);
                    egui::Frame::none()
                        .fill(egui::Color32::from_gray(60))
                        .inner_margin(6.0)
                        .show(ui, |ui| {
                            ui.set_min_width(full_width - 12.0);
                            egui::ScrollArea::vertical()
                                .stick_to_bottom(true)
                                .max_height(180.0)
                                .show(ui, |ui| {
                                    ui.set_min_width(full_width - 24.0);
                                    ui.set_min_height(160.0);
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
            if header_response.header_response.clicked() {
                self.logs_open = !self.logs_open;
            }
            }); // конец ScrollArea
        });

        // Автоподгонка размера окна под реальный размер содержимого —
        // но НЕ на каждом кадре. Раньше мы сравнивали used_size() каждый
        // кадр, и малейшее дрожание в доли пикселя (шрифты, скролл) снова
        // и снова слало команду на resize — а если в этот момент окно
        // ещё и перетаскивали мышкой, resize-команда конфликтовала с
        // системным перемещением окна, отсюда и лаг/желе при переносе.
        // Теперь пересчитываем размер только когда реально поменялось
        // что-то, что влияет на высоту (число участников, открыт ли лог).
        let signature = (peer_count, self.logs_open);
        if signature != self.last_layout_signature {
            self.last_layout_signature = signature;
            let used = ctx.used_size();
            let target = egui::vec2(WINDOW_WIDTH, used.y.min(MAX_WINDOW_HEIGHT).max(MIN_WINDOW_HEIGHT));
            if (target - self.last_window_size).length() > 1.0 {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(target));
                self.last_window_size = target;
            }
        }
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


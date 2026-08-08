use crate::network::Peer;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

pub type PeerList = Arc<Mutex<HashMap<SocketAddr, Peer>>>;

/// Максимум строк, которые хранятся в панели логов (старые обрезаются).
const MAX_LOG_LINES: usize = 500;

/// Всё, что нужно "расшарить" между GUI-потоком и фоновыми потоками
/// (сеть, захват/воспроизведение звука). Клонирование дёшево — это просто
/// набор Arc-указателей на одни и те же данные.
#[derive(Clone)]
pub struct SharedState {
    /// Известные участники сети (обновляется потоком discovery)
    pub peers: PeerList,
    /// Индивидуальная громкость каждого собеседника: 0.0 = мьют, 1.0 = как есть.
    /// Ключ — voice_addr пира (тот же, что в Peer).
    pub peer_gains: Arc<Mutex<HashMap<SocketAddr, f32>>>,
    /// Громкость собственного микрофона: 0.0..=2.0, по умолчанию 1.0.
    pub mic_gain: Arc<Mutex<f32>>,
    /// Мьют микрофона (полностью не отправлять звук)
    pub mic_muted: Arc<AtomicBool>,
    /// Мьют всего входящего звука (не воспроизводить ничего от собеседников)
    pub sound_muted: Arc<AtomicBool>,
    /// Кольцевой буфер логов для отображения в GUI
    pub logs: Arc<Mutex<VecDeque<String>>>,
    /// Флаг "сейчас должны работать сетевые/аудио потоки". Потоки проверяют
    /// его периодически (через таймаут на recv) и завершаются, когда он
    /// становится false — так реализовано отключение по кнопке.
    pub running: Arc<AtomicBool>,
}

impl SharedState {
    pub fn new() -> Self {
        SharedState {
            peers: Arc::new(Mutex::new(HashMap::new())),
            peer_gains: Arc::new(Mutex::new(HashMap::new())),
            mic_gain: Arc::new(Mutex::new(1.0)),
            mic_muted: Arc::new(AtomicBool::new(false)),
            sound_muted: Arc::new(AtomicBool::new(false)),
            logs: Arc::new(Mutex::new(VecDeque::new())),
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Добавляет строку в лог (используется вместо println!/eprintln! по
    /// всему проекту, чтобы сообщения были видны в GUI, а не терялись
    /// в невидимой пользователю консоли).
    pub fn log(&self, msg: impl Into<String>) {
        let mut logs = self.logs.lock().unwrap();
        logs.push_back(msg.into());
        while logs.len() > MAX_LOG_LINES {
            logs.pop_front();
        }
    }

    /// Возвращает громкость для конкретного пира (1.0, если ещё не задавали)
    pub fn peer_gain(&self, addr: &SocketAddr) -> f32 {
        *self.peer_gains.lock().unwrap().get(addr).unwrap_or(&1.0)
    }

    pub fn set_peer_gain(&self, addr: SocketAddr, gain: f32) {
        self.peer_gains.lock().unwrap().insert(addr, gain);
    }
}

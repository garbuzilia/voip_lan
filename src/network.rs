use crate::settings;
use crate::state::SharedState;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Информация об одном известном собеседнике в сети.
#[derive(Debug, Clone)]
pub struct Peer {
    pub username: String,
    pub voice_addr: SocketAddr,
    pub last_seen: Instant,
}

const MAGIC: &[u8] = b"VOIPLAN1"; // метка протокола discovery, чтобы отличать свои пакеты от мусора

/// Случайный ID текущего сеанса подключения — вставляется в каждый announce
/// и позволяет отличить "это я сам" от настоящего другого участника,
/// даже если у обоих совпадает имя пользователя. Генерируется заново
/// при каждом подключении (не глобальный статик), чтобы переподключение
/// после смены имени тоже работало корректно.
fn new_session_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64) << 32
}

/// Запускает discovery: параллельно рассылает broadcast "я тут" и слушает
/// такие же сообщения от других участников в сети. Работает без
/// центрального сервера — каждый клиент равноправен (mesh).
///
/// Оба потока проверяют `state.running` и завершаются, когда флаг сбрасывают
/// в false (используется UDP-сокет с таймаутом на чтение, чтобы поток не
/// "застревал" в блокирующем recv_from навсегда).
pub fn start_discovery(username: String, broadcast_addr: String, state: SharedState) -> Vec<JoinHandle<()>> {
    let my_session = new_session_id();
    let mut handles = Vec::new();

    // Поток-слушатель: принимает broadcast-пакеты от остальных
    let state_listener = state.clone();
    handles.push(thread::spawn(move || {
        let socket = match UdpSocket::bind(("0.0.0.0", settings::DISCOVERY_PORT)) {
            Ok(s) => s,
            Err(e) => {
                state_listener.log(format!("Не удалось открыть discovery-порт {}: {e}", settings::DISCOVERY_PORT));
                return;
            }
        };
        socket.set_broadcast(true).ok();
        socket.set_read_timeout(Some(Duration::from_millis(500))).ok();

        let mut buf = [0u8; 512];
        while state_listener.running.load(Ordering::Relaxed) {
            match socket.recv_from(&mut buf) {
                Ok((len, src)) => {
                    if let Some((peer, sender_session)) = parse_announce(&buf[..len], src) {
                        if sender_session == my_session {
                            continue; // это наш собственный broadcast, игнорируем
                        }
                        let mut map = state_listener.peers.lock().unwrap();
                        let is_new = !map.contains_key(&peer.voice_addr);
                        if is_new {
                            state_listener.log(format!("Новый участник в сети: {} ({})", peer.username, peer.voice_addr));
                        }
                        map.insert(peer.voice_addr, peer);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                    continue; // просто таймаут — идём на новый круг и проверяем running
                }
                Err(e) => {
                    state_listener.log(format!("Ошибка приёма discovery-пакета: {e}"));
                }
            }
        }
    }));

    // Поток-глашатай: периодически объявляет о своём присутствии broadcast'ом
    let state_announcer = state.clone();
    handles.push(thread::spawn(move || {
        let socket = match UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(e) => {
                state_announcer.log(format!("Не удалось создать сокет для announce: {e}"));
                return;
            }
        };
        socket.set_broadcast(true).ok();

        let dest = format!("{}:{}", broadcast_addr, settings::DISCOVERY_PORT);
        let payload = build_announce(&username, my_session);

        while state_announcer.running.load(Ordering::Relaxed) {
            if let Err(e) = socket.send_to(&payload, &dest) {
                state_announcer.log(format!("Не удалось отправить announce на {dest}: {e}"));
            }
            // Спим короткими интервалами, чтобы быстро реагировать на отключение,
            // а не ждать полный discovery_interval_secs после нажатия "Отключиться".
            let mut waited = Duration::ZERO;
            let step = Duration::from_millis(200);
            while waited < Duration::from_secs(settings::DISCOVERY_INTERVAL_SECS) {
                if !state_announcer.running.load(Ordering::Relaxed) {
                    return;
                }
                thread::sleep(step);
                waited += step;
            }
        }
    }));

    handles
}

/// Собирает пакет вида: MAGIC + session_id(8 байт) + длина_имени(1 байт) + имя_utf8
fn build_announce(username: &str, session: u64) -> Vec<u8> {
    let name_bytes = username.as_bytes();
    let mut packet = Vec::with_capacity(MAGIC.len() + 8 + 1 + name_bytes.len());
    packet.extend_from_slice(MAGIC);
    packet.extend_from_slice(&session.to_le_bytes());
    packet.push(name_bytes.len().min(255) as u8);
    packet.extend_from_slice(&name_bytes[..name_bytes.len().min(255)]);
    packet
}

/// Разбирает входящий announce-пакет. Возвращает None, если это не наш протокол.
/// Вместе с Peer возвращает session_id отправителя — по нему вызывающий код
/// отфильтровывает собственные объявления.
fn parse_announce(data: &[u8], src: SocketAddr) -> Option<(Peer, u64)> {
    let header_len = MAGIC.len() + 8 + 1;
    if data.len() < header_len || &data[..MAGIC.len()] != MAGIC {
        return None;
    }
    let session = u64::from_le_bytes(data[MAGIC.len()..MAGIC.len() + 8].try_into().ok()?);
    let name_len = data[MAGIC.len() + 8] as usize;
    let name_start = header_len;
    let name_end = name_start + name_len;
    if data.len() < name_end {
        return None;
    }
    let username = String::from_utf8_lossy(&data[name_start..name_end]).to_string();

    // Голосовой трафик от этого пира будем ждать с того же IP, но на voice_port
    let voice_addr = SocketAddr::new(src.ip(), settings::VOICE_PORT);

    Some((
        Peer {
            username,
            voice_addr,
            last_seen: Instant::now(),
        },
        session,
    ))
}

/// Убирает из списка пиров тех, от кого давно не было announce-пакетов.
/// Вызывать периодически (например, раз в секунду) из GUI-потока.
pub fn cleanup_stale_peers(state: &SharedState, timeout: Duration) {
    let mut map = state.peers.lock().unwrap();
    let before = map.len();
    map.retain(|_, peer| peer.last_seen.elapsed() < timeout);
    let removed = before - map.len();
    if removed > 0 {
        state.log(format!("Убрано неактивных участников: {removed}"));
    }
}

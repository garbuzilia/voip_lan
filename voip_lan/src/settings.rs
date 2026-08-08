use std::time::{SystemTime, UNIX_EPOCH};

// Все "настройки" — это просто константы в коде. Единственное, что
// реально нужно менять от пользователя к пользователю — это имя,
// и оно спрашивается в интерактивном режиме при запуске (см. main.rs).
// Никакого config.toml, никакого реестра — только один exe-файл.

/// Порт для голосового трафика (Opus-пакеты)
pub const VOICE_PORT: u16 = 47531;

/// Порт для обнаружения участников (discovery broadcast)
pub const DISCOVERY_PORT: u16 = 47532;

/// Как часто рассылать "я тут" в сеть
pub const DISCOVERY_INTERVAL_SECS: u64 = 3;

/// Через сколько секунд неактивности считать участника отключившимся
pub const PEER_TIMEOUT_SECS: u64 = 10;

/// Опус работает только с фиксированными частотами: 8000/12000/16000/24000/48000.
/// Mumble по умолчанию использует 48 кГц моно — делаем так же.
pub const SAMPLE_RATE: u32 = 48000;

/// 10 мс на 48 кГц = 480 сэмплов. Это тот же размер фрейма, что использует
/// Mumble по умолчанию (можно увеличить до 20/40/60 мс, но чем крупнее
/// фрейм, тем выше задержка).
pub const FRAME_SIZE: usize = 480;

/// Битрейт голосового потока. У Mumble по умолчанию ~40 кбит/с для
/// голоса в режиме VoIP — берём то же самое.
pub const OPUS_BITRATE: i32 = 40_000;

/// Общий "адрес неограниченного broadcast" — уходит через интерфейс
/// операционной системы по умолчанию. Для Radmin VPN обычно этого
/// достаточно, но если участники не находят друг друга, можно явно
/// указать адрес подсети Radmin (обычно 26.x.x.255) первым аргументом
/// командной строки: voip_lan.exe MyName 26.155.20.255
pub const DEFAULT_BROADCAST_ADDR: &str = "255.255.255.255";

/// Определяет имя пользователя:
/// 1. Если передано как аргумент командной строки — используем его.
/// 2. Иначе спрашиваем в консоли (Enter — пропустить).
/// 3. Если ничего не введено — генерируем "UserNNNN" на основе текущего времени.
pub fn resolve_username(cli_arg: Option<String>) -> String {
    if let Some(name) = cli_arg {
        if !name.trim().is_empty() {
            return name.trim().to_string();
        }
    }

    print!("Введите ваше имя (Enter — использовать случайное): ");
    use std::io::Write;
    std::io::stdout().flush().ok();

    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_ok() {
        let trimmed = input.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    generate_random_username()
}

fn generate_random_username() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("User{}", nanos % 10000)
}

use crate::settings;
use crate::state::SharedState;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use opus::{Application, Channels, Decoder as OpusDecoder, Encoder as OpusEncoder};
use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const VOICE_MAGIC: &[u8] = b"VLV2";

// ============================================================================
// Перечисление доступных устройств (для выпадающих списков в GUI)
// ============================================================================

pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    host.input_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

pub fn list_output_devices() -> Vec<String> {
    let host = cpal::default_host();
    host.output_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

pub fn default_input_device_name() -> Option<String> {
    cpal::default_host().default_input_device()?.name().ok()
}

pub fn default_output_device_name() -> Option<String> {
    cpal::default_host().default_output_device()?.name().ok()
}

fn find_input_device(name: &str) -> Option<cpal::Device> {
    let host = cpal::default_host();
    host.input_devices().ok()?.find(|d| d.name().map(|n| n == name).unwrap_or(false))
}

fn find_output_device(name: &str) -> Option<cpal::Device> {
    let host = cpal::default_host();
    host.output_devices().ok()?.find(|d| d.name().map(|n| n == name).unwrap_or(false))
}

// ============================================================================
// Ресемплеры (см. пояснение в предыдущей версии): переводят звук с "родной"
// частоты устройства на 48 кГц, которых требует Opus (как и у Mumble), и обратно.
// ============================================================================

struct PushResampler {
    ratio: f64,
    pos: f64,
    carry: Vec<i16>,
}

impl PushResampler {
    fn new(src_rate: u32, dst_rate: u32) -> Self {
        Self { ratio: src_rate as f64 / dst_rate as f64, pos: 0.0, carry: Vec::new() }
    }

    fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        self.carry.extend_from_slice(input);
        loop {
            let idx = self.pos.floor() as usize;
            if idx + 1 >= self.carry.len() {
                break;
            }
            let frac = self.pos - idx as f64;
            let s0 = self.carry[idx] as f64;
            let s1 = self.carry[idx + 1] as f64;
            let sample = s0 + (s1 - s0) * frac;
            out.push(sample.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
            self.pos += self.ratio;
        }
        let drop_count = self.pos.floor() as usize;
        if drop_count > 0 && drop_count <= self.carry.len() {
            self.carry.drain(0..drop_count);
            self.pos -= drop_count as f64;
        }
    }
}

struct PullResampler<F: FnMut() -> i16> {
    source: F,
    ratio: f64,
    pos: f64,
    prev: i16,
    curr: i16,
}

impl<F: FnMut() -> i16> PullResampler<F> {
    fn new(mut source: F, src_rate: u32, dst_rate: u32) -> Self {
        let prev = source();
        let curr = source();
        Self { source, ratio: src_rate as f64 / dst_rate as f64, pos: 0.0, prev, curr }
    }

    fn next(&mut self) -> i16 {
        let out = self.prev as f64 + (self.curr as f64 - self.prev as f64) * self.pos;
        self.pos += self.ratio;
        while self.pos >= 1.0 {
            self.pos -= 1.0;
            self.prev = self.curr;
            self.curr = (self.source)();
        }
        out.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16
    }
}

// ============================================================================
// Входящий голос от собеседников: свой Opus-декодер на каждого (у декодера
// есть внутреннее состояние) + очередь декодированных сэмплов на микширование.
// ============================================================================

struct PeerVoice {
    decoder: OpusDecoder,
    pcm: VecDeque<i16>,
    last_packet: Instant,
}

type PeerAudioMap = Arc<Mutex<HashMap<SocketAddr, PeerVoice>>>;

/// Честный микшер: берёт по одному сэмплу из очереди каждого активного
/// собеседника, умножает на его персональную громкость и складывает —
/// а не просто сваливает куски в одну очередь друг за другом.
fn pull_mixed_sample(peers_audio: &PeerAudioMap, state: &SharedState) -> i16 {
    if state.sound_muted.load(Ordering::Relaxed) {
        // Даже когда звук замьючен, продолжаем вычерпывать очереди,
        // чтобы при размьюте не воспроизвести резко накопившуюся задержку.
        let mut map = peers_audio.lock().unwrap();
        for voice in map.values_mut() {
            voice.pcm.pop_front();
        }
        return 0;
    }

    let mut map = peers_audio.lock().unwrap();
    let mut sum: i32 = 0;
    for (addr, voice) in map.iter_mut() {
        if let Some(s) = voice.pcm.pop_front() {
            let gain = state.peer_gain(addr);
            sum += (s as f32 * gain) as i32;
        }
    }
    sum.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

// ============================================================================
// Публичный API: запуск/остановка голосового тракта на время "подключения"
// ============================================================================

/// Хендлы всего, что нужно держать живым, пока идёт голосовой чат.
/// `stop()` останавливает именно голосовую сессию (звук + её сетевые
/// потоки), не трогая discovery — это позволяет менять устройство
/// микрофона/наушников на лету, не отключая остальных участников.
pub struct VoiceHandles {
    /// None, если голосовая сессия запущена без микрофона (например,
    /// пользователь хочет просто слушать канал) — захват звука тогда
    /// не запускается вовсе, а не просто "молчит".
    pub input_stream: Option<cpal::Stream>,
    pub output_stream: cpal::Stream,
    pub network_threads: Vec<JoinHandle<()>>,
    /// Отдельный от `SharedState::running` флаг: управляет только сетевыми
    /// потоками голоса (отправка/приём/чистка), НЕ discovery. Так можно
    /// перезапустить звук при смене устройства, не отключая остальных
    /// участников от обнаружения.
    pub voice_running: Arc<AtomicBool>,
}

impl VoiceHandles {
    /// Останавливает голосовую сессию: сначала дропает потоки звука
    /// (это разрывает канал tx -> send-поток и останавливает callback'и),
    /// затем сигналит сетевым потокам завершиться и дожидается их (join),
    /// чтобы UDP-порт точно освободился перед повторным start_voice.
    pub fn stop(self) {
        drop(self.input_stream);
        drop(self.output_stream);
        self.voice_running.store(false, Ordering::Relaxed);
        for handle in self.network_threads {
            let _ = handle.join();
        }
    }
}

pub fn start_voice(
    state: SharedState,
    input_device_name: &str,
    output_device_name: &str,
) -> Result<VoiceHandles, String> {
    // Микрофон опционален: пустая строка = "без микрофона", осознанный
    // выбор пользователя (или единственный вариант, если устройств ввода
    // в системе вообще нет). В этом случае просто не поднимаем захват
    // звука — приём и воспроизведение чужого голоса работают как обычно.
    let input_device = if input_device_name.is_empty() {
        None
    } else {
        Some(
            find_input_device(input_device_name)
                .ok_or_else(|| format!("Устройство ввода не найдено: {input_device_name}"))?,
        )
    };
    let output_device = find_output_device(output_device_name)
        .ok_or_else(|| format!("Устройство вывода не найдено: {output_device_name}"))?;

    let voice_socket = UdpSocket::bind(("0.0.0.0", settings::VOICE_PORT))
        .map_err(|e| format!("Не удалось открыть voice-порт {}: {e}", settings::VOICE_PORT))?;
    voice_socket
        .set_read_timeout(Some(Duration::from_millis(500)))
        .map_err(|e| e.to_string())?;

    let voice_running = Arc::new(AtomicBool::new(true));
    let mut network_threads = Vec::new();

    // --- Отправка голоса: захват -> канал -> сеть (только если есть микрофон) ---
    let input_stream = if let Some(input_device) = input_device {
        let (tx, rx) = channel::<Vec<u8>>();

        let send_socket = voice_socket.try_clone().map_err(|e| e.to_string())?;
        let state_for_send = state.clone();
        let voice_running_send = Arc::clone(&voice_running);
        network_threads.push(thread::spawn(move || {
            while voice_running_send.load(Ordering::Relaxed) {
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(opus_frame) => {
                        let mut packet = Vec::with_capacity(VOICE_MAGIC.len() + opus_frame.len());
                        packet.extend_from_slice(VOICE_MAGIC);
                        packet.extend_from_slice(&opus_frame);

                        let targets: Vec<_> = state_for_send.peers.lock().unwrap().values().cloned().collect();
                        for peer in targets {
                            let _ = send_socket.send_to(&packet, peer.voice_addr);
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        }));

        Some(build_input_stream(&input_device, state.clone(), tx)?)
    } else {
        state.log("Подключение без микрофона — только приём звука");
        None
    };

    // --- Приём голоса: сеть -> Opus-декодер (свой на пира) -> очередь пира ---
    let peers_audio: PeerAudioMap = Arc::new(Mutex::new(HashMap::new()));

    let recv_socket = voice_socket;
    let peers_audio_for_recv = Arc::clone(&peers_audio);
    let state_for_recv = state.clone();
    let voice_running_recv = Arc::clone(&voice_running);
    network_threads.push(thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while voice_running_recv.load(Ordering::Relaxed) {
            match recv_socket.recv_from(&mut buf) {
                Ok((len, src)) => {
                    if len < VOICE_MAGIC.len() || &buf[..VOICE_MAGIC.len()] != VOICE_MAGIC {
                        continue;
                    }
                    let opus_data = &buf[VOICE_MAGIC.len()..len];

                    let mut map = peers_audio_for_recv.lock().unwrap();
                    let voice = map.entry(src).or_insert_with(|| PeerVoice {
                        decoder: OpusDecoder::new(settings::SAMPLE_RATE, Channels::Mono)
                            .expect("не удалось создать Opus-декодер"),
                        pcm: VecDeque::new(),
                        last_packet: Instant::now(),
                    });

                    let mut pcm_out = vec![0i16; settings::FRAME_SIZE];
                    match voice.decoder.decode(opus_data, &mut pcm_out, false) {
                        Ok(decoded_len) => {
                            voice.pcm.extend(&pcm_out[..decoded_len]);
                            voice.last_packet = Instant::now();
                            while voice.pcm.len() > settings::SAMPLE_RATE as usize {
                                voice.pcm.pop_front();
                            }
                        }
                        Err(e) => {
                            state_for_recv.log(format!("Ошибка декодирования Opus-пакета от {src}: {e}"));
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                    continue;
                }
                Err(e) => {
                    state_for_recv.log(format!("Ошибка приёма голосового пакета: {e}"));
                }
            }
        }
    }));

    // Периодическая чистка декодеров собеседников, от которых давно нет пакетов
    let peers_audio_for_cleanup = Arc::clone(&peers_audio);
    let voice_running_cleanup = Arc::clone(&voice_running);
    network_threads.push(thread::spawn(move || {
        while voice_running_cleanup.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(500));
            let mut map = peers_audio_for_cleanup.lock().unwrap();
            map.retain(|_, voice| voice.last_packet.elapsed() < Duration::from_secs(settings::PEER_TIMEOUT_SECS));
        }
    }));

    let output_stream = build_output_stream(&output_device, state, peers_audio)?;

    Ok(VoiceHandles { input_stream, output_stream, network_threads, voice_running })
}

/// Захват микрофона: даунмикс в моно + ресемплинг до 48 кГц + буферизация
/// во фреймы по 480 сэмплов (10 мс, как у Mumble) + громкость/мьют +
/// кодирование в Opus + отправка в канал на сетевой поток.
fn build_input_stream(device: &cpal::Device, state: SharedState, tx: Sender<Vec<u8>>) -> Result<cpal::Stream, String> {
    let supported_config = device
        .default_input_config()
        .map_err(|e| format!("Нет доступной конфигурации микрофона: {e}"))?;
    let sample_format = supported_config.sample_format();
    let stream_config: cpal::StreamConfig = supported_config.into();
    let channels = stream_config.channels as usize;
    let native_rate = stream_config.sample_rate.0;

    match sample_format {
        SampleFormat::I16 => {
            build_input_stream_typed::<i16>(device, &stream_config, channels, native_rate, state, tx, downmix_i16)
        }
        SampleFormat::F32 => {
            build_input_stream_typed::<f32>(device, &stream_config, channels, native_rate, state, tx, downmix_f32)
        }
        other => Err(format!("Неподдерживаемый формат сэмплов микрофона: {other:?}")),
    }
}

fn build_input_stream_typed<T>(
    device: &cpal::Device,
    stream_config: &cpal::StreamConfig,
    channels: usize,
    native_rate: u32,
    state: SharedState,
    tx: Sender<Vec<u8>>,
    downmix: fn(&[T], usize) -> Vec<i16>,
) -> Result<cpal::Stream, String>
where
    T: cpal::Sample + cpal::SizedSample + Send + 'static,
{
    let mut encoder = OpusEncoder::new(settings::SAMPLE_RATE, Channels::Mono, Application::Voip)
        .map_err(|e| format!("Не удалось создать Opus-энкодер: {e}"))?;
    encoder.set_bitrate(opus::Bitrate::Bits(settings::OPUS_BITRATE)).ok();

    let mut resampler = PushResampler::new(native_rate, settings::SAMPLE_RATE);
    let mut frame_accum: Vec<i16> = Vec::with_capacity(settings::FRAME_SIZE * 2);
    let mut resampled: Vec<i16> = Vec::new();
    let mut opus_out = vec![0u8; 4000];

    let err_fn = |err| eprintln!("Ошибка потока захвата звука: {err}");

    let stream = device
        .build_input_stream(
            stream_config,
            move |data: &[T], _| {
                if state.mic_muted.load(Ordering::Relaxed) {
                    return; // микрофон замьючен — ничего не кодируем и не шлём
                }

                let mut mono = downmix(data, channels);
                let gain = *state.mic_gain.lock().unwrap();
                if (gain - 1.0).abs() > f32::EPSILON {
                    for s in mono.iter_mut() {
                        *s = (*s as f32 * gain).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                    }
                }

                resampled.clear();
                resampler.process(&mono, &mut resampled);
                frame_accum.extend_from_slice(&resampled);

                while frame_accum.len() >= settings::FRAME_SIZE {
                    let frame: Vec<i16> = frame_accum.drain(..settings::FRAME_SIZE).collect();
                    match encoder.encode(&frame, &mut opus_out) {
                        Ok(len) => {
                            let _ = tx.send(opus_out[..len].to_vec());
                        }
                        Err(e) => eprintln!("Ошибка кодирования Opus: {e}"),
                    }
                }
            },
            err_fn,
            None,
        )
        .map_err(|e| format!("Не удалось создать поток захвата: {e}"))?;

    stream.play().map_err(|e| format!("Не удалось запустить захват звука: {e}"))?;
    Ok(stream)
}

/// Воспроизведение: тянет уже смикшированный (сумма всех говорящих,
/// с учётом персональной громкости и общего мьюта) сигнал на 48 кГц
/// через ресемплер до частоты устройства и дублирует в нужное число каналов.
fn build_output_stream(device: &cpal::Device, state: SharedState, peers_audio: PeerAudioMap) -> Result<cpal::Stream, String> {
    let supported_config = device
        .default_output_config()
        .map_err(|e| format!("Нет доступной конфигурации динамиков: {e}"))?;
    let sample_format = supported_config.sample_format();
    let stream_config: cpal::StreamConfig = supported_config.into();
    let channels = stream_config.channels as usize;
    let native_rate = stream_config.sample_rate.0;

    let source = move || pull_mixed_sample(&peers_audio, &state);
    let mut resampler = PullResampler::new(source, settings::SAMPLE_RATE, native_rate);

    let err_fn = |err| eprintln!("Ошибка потока воспроизведения: {err}");

    let stream = match sample_format {
        SampleFormat::I16 => device.build_output_stream(
            &stream_config,
            move |data: &mut [i16], _| {
                for frame in data.chunks_mut(channels) {
                    let s = resampler.next();
                    for out in frame.iter_mut() {
                        *out = s;
                    }
                }
            },
            err_fn,
            None,
        ),
        SampleFormat::F32 => device.build_output_stream(
            &stream_config,
            move |data: &mut [f32], _| {
                for frame in data.chunks_mut(channels) {
                    let s = resampler.next() as f32 / i16::MAX as f32;
                    for out in frame.iter_mut() {
                        *out = s;
                    }
                }
            },
            err_fn,
            None,
        ),
        other => return Err(format!("Неподдерживаемый формат сэмплов динамиков: {other:?}")),
    }
    .map_err(|e| format!("Не удалось создать поток воспроизведения: {e}"))?;

    stream.play().map_err(|e| format!("Не удалось запустить воспроизведение: {e}"))?;
    Ok(stream)
}

fn downmix_i16(data: &[i16], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return data.to_vec();
    }
    data.chunks(channels)
        .map(|frame| {
            let sum: i32 = frame.iter().map(|&s| s as i32).sum();
            (sum / channels as i32) as i16
        })
        .collect()
}

fn downmix_f32(data: &[f32], channels: usize) -> Vec<i16> {
    let to_i16 = |s: f32| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
    if channels <= 1 {
        return data.iter().map(|&s| to_i16(s)).collect();
    }
    data.chunks(channels)
        .map(|frame| {
            let sum: f32 = frame.iter().sum();
            to_i16(sum / channels as f32)
        })
        .collect()
}

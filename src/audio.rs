use crate::network::PeerList;
use crate::settings;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use opus::{Application, Channels, Decoder as OpusDecoder, Encoder as OpusEncoder};
use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const VOICE_MAGIC: &[u8] = b"VLV2"; // метка voice-пакетов (v2 = формат с Opus)

// ============================================================================
// Ресемплер: переводит звук с "родной" частоты устройства на нужную нам
// (48 кГц, как использует Mumble) и обратно. Простая линейная интерполяция —
// не аудиофильское качество, но для голоса более чем достаточно и не требует
// внешних библиотек.
// ============================================================================

/// Push-ресемплер: скармливаем ему сэмплы по мере поступления (из callback'а
/// захвата звука), он копит хвост между вызовами и отдаёт всё, что успел
/// пересчитать на новую частоту.
struct PushResampler {
    ratio: f64, // во сколько раз входная частота больше выходной
    pos: f64,
    carry: Vec<i16>,
}

impl PushResampler {
    fn new(src_rate: u32, dst_rate: u32) -> Self {
        Self {
            ratio: src_rate as f64 / dst_rate as f64,
            pos: 0.0,
            carry: Vec::new(),
        }
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

        // Чистим "съеденный" хвост, чтобы carry не рос бесконечно
        let drop_count = self.pos.floor() as usize;
        if drop_count > 0 && drop_count <= self.carry.len() {
            self.carry.drain(0..drop_count);
            self.pos -= drop_count as f64;
        }
    }
}

/// Pull-ресемплер: используется на воспроизведении. В отличие от push-варианта,
/// сам "тянет" сэмплы из источника (микшера) по мере необходимости —
/// удобно, потому что cpal требует заполнить ровно `data.len()` элементов
/// в callback'е воспроизведения, и мы не знаем заранее, сколько микшер
/// сможет отдать.
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
        Self {
            source,
            ratio: src_rate as f64 / dst_rate as f64,
            pos: 0.0,
            prev,
            curr,
        }
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
// Состояние входящего голоса от одного собеседника: свой Opus-декодер
// (у декодера есть внутреннее состояние, поэтому на каждого пира — отдельный
// экземпляр) и очередь уже декодированных сэмплов, ждущих микширования.
// ============================================================================

struct PeerVoice {
    decoder: OpusDecoder,
    pcm: VecDeque<i16>,
    last_packet: Instant,
}

type PeerAudioMap = Arc<Mutex<HashMap<SocketAddr, PeerVoice>>>;

/// Честный микшер: берёт по одному сэмплу из очереди КАЖДОГО активного
/// собеседника и складывает их вместе (с ограничением, чтобы не было
/// переполнения при клиппинге), а не просто ставит куски в общую очередь
/// друг за другом, как было в первой версии.
fn pull_mixed_sample(peers_audio: &PeerAudioMap) -> i16 {
    let mut map = peers_audio.lock().unwrap();
    let mut sum: i32 = 0;
    for state in map.values_mut() {
        if let Some(s) = state.pcm.pop_front() {
            sum += s as i32;
        }
    }
    sum.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

// ============================================================================

pub fn start_audio(username: String, peers: PeerList) -> Result<(), String> {
    let host = cpal::default_host();

    let input_device = host
        .default_input_device()
        .ok_or("Не найдено устройство ввода звука (микрофон)")?;
    let output_device = host
        .default_output_device()
        .ok_or("Не найдено устройство вывода звука (динамики)")?;

    println!("Микрофон: {}", input_device.name().unwrap_or_default());
    println!("Динамики: {}", output_device.name().unwrap_or_default());
    let _ = username; // имя уже используется в discovery, тут не требуется

    // --- Отправка голоса: захват -> ресемплинг -> Opus -> канал -> сеть ---
    let (tx, rx) = channel::<Vec<u8>>();
    build_input_stream(&input_device, tx)?;

    let voice_socket = UdpSocket::bind(("0.0.0.0", settings::VOICE_PORT))
        .map_err(|e| format!("Не удалось открыть voice-порт {}: {e}", settings::VOICE_PORT))?;

    let send_socket = voice_socket.try_clone().map_err(|e| e.to_string())?;
    let peers_for_send = Arc::clone(&peers);
    thread::spawn(move || {
        for opus_frame in rx {
            let mut packet = Vec::with_capacity(VOICE_MAGIC.len() + opus_frame.len());
            packet.extend_from_slice(VOICE_MAGIC);
            packet.extend_from_slice(&opus_frame);

            let targets: Vec<_> = peers_for_send.lock().unwrap().values().cloned().collect();
            for peer in targets {
                let _ = send_socket.send_to(&packet, peer.voice_addr);
            }
        }
    });

    // --- Приём голоса: сеть -> Opus-декодер (свой на пира) -> очередь пира ---
    let peers_audio: PeerAudioMap = Arc::new(Mutex::new(HashMap::new()));
    let recv_socket = voice_socket;
    let peers_audio_for_recv = Arc::clone(&peers_audio);
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match recv_socket.recv_from(&mut buf) {
                Ok((len, src)) => {
                    if len < VOICE_MAGIC.len() || &buf[..VOICE_MAGIC.len()] != VOICE_MAGIC {
                        continue;
                    }
                    let opus_data = &buf[VOICE_MAGIC.len()..len];

                    let mut map = peers_audio_for_recv.lock().unwrap();
                    let state = map.entry(src).or_insert_with(|| PeerVoice {
                        decoder: OpusDecoder::new(settings::SAMPLE_RATE, Channels::Mono)
                            .expect("не удалось создать Opus-декодер"),
                        pcm: VecDeque::new(),
                        last_packet: Instant::now(),
                    });

                    let mut pcm_out = vec![0i16; settings::FRAME_SIZE];
                    match state.decoder.decode(opus_data, &mut pcm_out, false) {
                        Ok(decoded_len) => {
                            state.pcm.extend(&pcm_out[..decoded_len]);
                            state.last_packet = Instant::now();
                            // Не даём очереди одного пира расти бесконечно,
                            // если микшер по какой-то причине не успевает её вычерпывать
                            while state.pcm.len() > settings::SAMPLE_RATE as usize {
                                state.pcm.pop_front();
                            }
                        }
                        Err(e) => {
                            eprintln!("Ошибка декодирования Opus-пакета от {src}: {e}");
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Ошибка приёма голосового пакета: {e}");
                }
            }
        }
    });

    // Периодически убираем декодеры собеседников, от которых давно нет пакетов
    let peers_audio_for_cleanup = Arc::clone(&peers_audio);
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(5));
        let mut map = peers_audio_for_cleanup.lock().unwrap();
        map.retain(|_, state| state.last_packet.elapsed() < Duration::from_secs(settings::PEER_TIMEOUT_SECS));
    });

    build_output_stream(&output_device, peers_audio)?;

    Ok(())
}

/// Настраивает поток захвата с микрофона: даунмикс в моно (если каналов
/// больше одного) + ресемплинг до 48 кГц + буферизация во фреймы по 480
/// сэмплов (10 мс, как у Mumble) + кодирование в Opus + отправка в канал.
fn build_input_stream(device: &cpal::Device, tx: Sender<Vec<u8>>) -> Result<(), String> {
    let supported_config = device
        .default_input_config()
        .map_err(|e| format!("Нет доступной конфигурации микрофона: {e}"))?;
    let sample_format = supported_config.sample_format();
    let stream_config: cpal::StreamConfig = supported_config.into();
    let channels = stream_config.channels as usize;
    let native_rate = stream_config.sample_rate.0;

    let mut encoder = OpusEncoder::new(settings::SAMPLE_RATE, Channels::Mono, Application::Voip)
        .map_err(|e| format!("Не удалось создать Opus-энкодер: {e}"))?;
    encoder
        .set_bitrate(opus::Bitrate::Bits(settings::OPUS_BITRATE))
        .ok();

    let mut resampler = PushResampler::new(native_rate, settings::SAMPLE_RATE);
    let mut frame_accum: Vec<i16> = Vec::with_capacity(settings::FRAME_SIZE * 2);
    let mut resampled: Vec<i16> = Vec::new();
    let mut opus_out = vec![0u8; 4000]; // с запасом, реальный Opus-фрейм намного меньше

    let err_fn = |err| eprintln!("Ошибка потока захвата звука: {err}");

    let mut encode_and_send = move |mono: &[i16], tx: &Sender<Vec<u8>>| {
        resampled.clear();
        resampler.process(mono, &mut resampled);
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
    };

    let stream = match sample_format {
        SampleFormat::I16 => device.build_input_stream(
            &stream_config,
            move |data: &[i16], _| {
                let mono = downmix_i16(data, channels);
                encode_and_send(&mono, &tx);
            },
            err_fn,
            None,
        ),
        SampleFormat::F32 => {
            // Второй замыкающий блок не может переиспользовать `encode_and_send`
            // из ветки I16 (перемещено по move), поэтому пересобираем поток
            // захвата целиком под F32 с собственным набором состояний.
            return build_input_stream_f32(device, &stream_config, channels, tx);
        }
        other => return Err(format!("Неподдерживаемый формат сэмплов микрофона: {other:?}")),
    }
    .map_err(|e| format!("Не удалось создать поток захвата: {e}"))?;

    stream.play().map_err(|e| format!("Не удалось запустить захват звука: {e}"))?;
    std::mem::forget(stream); // поток должен жить всю программу
    Ok(())
}

/// Отдельная функция для формата F32 — избегает конфликта владения
/// замыканием между двумя ветками build_input_stream.
fn build_input_stream_f32(
    device: &cpal::Device,
    stream_config: &cpal::StreamConfig,
    channels: usize,
    tx: Sender<Vec<u8>>,
) -> Result<(), String> {
    let native_rate = stream_config.sample_rate.0;

    let mut encoder = OpusEncoder::new(settings::SAMPLE_RATE, Channels::Mono, Application::Voip)
        .map_err(|e| format!("Не удалось создать Opus-энкодер: {e}"))?;
    encoder
        .set_bitrate(opus::Bitrate::Bits(settings::OPUS_BITRATE))
        .ok();

    let mut resampler = PushResampler::new(native_rate, settings::SAMPLE_RATE);
    let mut frame_accum: Vec<i16> = Vec::with_capacity(settings::FRAME_SIZE * 2);
    let mut resampled: Vec<i16> = Vec::new();
    let mut opus_out = vec![0u8; 4000];

    let err_fn = |err| eprintln!("Ошибка потока захвата звука: {err}");

    let stream = device
        .build_input_stream(
            stream_config,
            move |data: &[f32], _| {
                let mono = downmix_f32(data, channels);
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
    std::mem::forget(stream);
    Ok(())
}

/// Настраивает поток воспроизведения: тянет уже смикшированный (сумма
/// всех говорящих) сигнал на 48 кГц через ресемплер до частоты устройства
/// и дублирует в нужное число каналов.
fn build_output_stream(device: &cpal::Device, peers_audio: PeerAudioMap) -> Result<(), String> {
    let supported_config = device
        .default_output_config()
        .map_err(|e| format!("Нет доступной конфигурации динамиков: {e}"))?;
    let sample_format = supported_config.sample_format();
    let stream_config: cpal::StreamConfig = supported_config.into();
    let channels = stream_config.channels as usize;
    let native_rate = stream_config.sample_rate.0;

    let source = move || pull_mixed_sample(&peers_audio);
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
    std::mem::forget(stream);
    Ok(())
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

mod audio;
mod network;
mod settings;

use network::PeerList;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

fn main() {
    println!("=== voip_lan: голосовой чат для локальной сети (Radmin VPN) ===\n");

    let mut args = std::env::args().skip(1);
    let cli_username = args.next();
    // Второй (необязательный) аргумент — адрес широковещательной рассылки,
    // на случай если общий 255.255.255.255 не доходит через VPN-адаптер.
    // Пример: voip_lan.exe MyName 26.155.20.255
    let broadcast_addr = args
        .next()
        .unwrap_or_else(|| settings::DEFAULT_BROADCAST_ADDR.to_string());

    let username = settings::resolve_username(cli_username);
    println!("\nИмя: {username}");
    println!(
        "Порты: голос {} / обнаружение {} | broadcast: {broadcast_addr}",
        settings::VOICE_PORT,
        settings::DISCOVERY_PORT
    );
    println!("(если участники не находят друг друга — укажите вторым аргументом");
    println!(" адрес подсети Radmin VPN, например: voip_lan.exe {username} 26.x.x.255)\n");

    let peers: PeerList = Arc::new(Mutex::new(HashMap::new()));

    network::start_discovery(username.clone(), broadcast_addr, Arc::clone(&peers));

    if let Err(e) = audio::start_audio(username, Arc::clone(&peers)) {
        eprintln!("Не удалось запустить аудио-подсистему: {e}");
        eprintln!("Программа продолжит работать в режиме discovery-only.");
    }

    loop {
        thread::sleep(Duration::from_secs(1));
        network::cleanup_stale_peers(&peers, Duration::from_secs(settings::PEER_TIMEOUT_SECS));

        let map = peers.lock().unwrap();
        if map.is_empty() {
            print!("\rУчастников не обнаружено...                              ");
        } else {
            let names: Vec<String> = map.values().map(|p| p.username.clone()).collect();
            print!("\rВ сети: {}                              ", names.join(", "));
        }
        use std::io::Write;
        std::io::stdout().flush().ok();
    }
}

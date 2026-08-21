// SPDX-License-Identifier: AGPL-3.0-only
//! **A/B ölçümü: TCP vs uTP** (F12). `utp_ab <infohash> [<infohash>…]`
//!
//! Gerekçe: gece boyu ölçümde peer denemelerinin %97'si TCP zaman aşımıydı
//! (130.948 / 134.391). Hipotez: peer'lerin çoğu TCP'ye kapalı ama uTP'ye (BEP-29)
//! açık — modern istemciler uTP'yi tercih eder ve NAT arkasındaki peer'ler pratikte
//! yalnız uTP ile erişilebilir. Bu araç aynı peer listesini iki aktarımla dener ve
//! başarı oranlarını karşılaştırır; tam entegrasyon ancak fark anlamlıysa yapılır.
use std::time::Duration;

use dragnet_core::InfoHash;
use dragnet_meta::{wire, FetchConfig, MetadataFetcher};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("kullanım: utp_ab <infohash> [<infohash>…]");
        std::process::exit(2);
    }
    let fetcher = MetadataFetcher::new(FetchConfig::default()).expect("fetcher");
    let boot = fetcher.wait_bootstrapped().await;
    println!("DHT bootstrap: {boot}");
    // Yönlendirme tablosunun dolması için kısa bekleme (ölçüm aracı; üretimde motor
    // zaten uzun süre ayakta).
    tokio::time::sleep(Duration::from_secs(10)).await;
    let utp = librqbit_utp::UtpSocket::new_udp("0.0.0.0:0".parse().unwrap())
        .await
        .expect("uTP soketi");

    let (mut tcp_ok, mut tcp_to, mut tcp_err) = (0u32, 0u32, 0u32);
    let (mut utp_ok, mut utp_to, mut utp_err) = (0u32, 0u32, 0u32);
    let mut peers_total = 0usize;

    for a in &args {
        let Some(ih) = InfoHash::from_hex(a) else {
            eprintln!("geçersiz infohash: {a}");
            continue;
        };
        let peers = fetcher.peers_of(ih, Duration::from_secs(25), 20).await;
        peers_total += peers.len();
        println!("\n{a}: {} peer bulundu", peers.len());
        for p in peers {
            let t = wire::fetch_info_from_peer(p, *ih.as_bytes(), Duration::from_secs(10)).await;
            let u =
                wire::fetch_info_from_peer_utp(&utp, p, *ih.as_bytes(), Duration::from_secs(10))
                    .await;
            let mark = |r: &Result<Vec<u8>, dragnet_meta::PeerError>| match r {
                Ok(b) => format!("OK ({} bayt)", b.len()),
                Err(e) => format!("{e}"),
            };
            println!("   {p}  TCP: {:<40} uTP: {}", mark(&t), mark(&u));
            match t {
                Ok(_) => tcp_ok += 1,
                Err(dragnet_meta::PeerError::Timeout) => tcp_to += 1,
                Err(_) => tcp_err += 1,
            }
            match u {
                Ok(_) => utp_ok += 1,
                Err(dragnet_meta::PeerError::Timeout) => utp_to += 1,
                Err(_) => utp_err += 1,
            }
        }
    }
    println!("\n=== SONUÇ ({peers_total} peer denemesi)");
    println!("TCP: {tcp_ok} başarılı · {tcp_to} zaman aşımı · {tcp_err} diğer hata");
    println!("uTP: {utp_ok} başarılı · {utp_to} zaman aşımı · {utp_err} diğer hata");
}

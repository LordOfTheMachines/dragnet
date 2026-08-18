// SPDX-License-Identifier: AGPL-3.0-only
//! Cihaz geneli ve süreç-ağacı GPU belleği — Windows performans sayaçları (PDH).
//!
//! DXGI `QueryVideoMemoryInfo` **süreç-yereldir**: yalnız bu sürecin kullanımını verir.
//! Kullanıcı ise "toplam ne kadar VRAM dolu, bunun ne kadarı bizim" ayrımını görmek ister.
//! Bunun için Görev Yöneticisi'nin de okuduğu sayaçlar kullanılır (salt-okuma, harici
//! süreç çalıştırmadan):
//!
//! - `\GPU Adapter Memory(luid_0x…_phys_N)\Dedicated Usage` → bağdaştırıcının toplam
//!   ayrılmış bellek kullanımı (tüm uygulamalar).
//! - `\GPU Process Memory(pid_… _luid_0x…_phys_N)\Dedicated Usage` → süreç başına kullanım;
//!   Dragnet'in payı için kendi PID'imiz **ve alt süreçlerimiz** (Tauri'de arayüz ayrı
//!   WebView2 süreçlerinde çalışır, GPU belleğini onlar tahsis eder) toplanır.
//!
//! Sayaçlar `PdhAddEnglishCounterW` ile eklenir: yerelleştirilmiş Windows'ta sayaç adları
//! çevrilidir, İngilizce API'si bu sorunu ortadan kaldırır.

use std::collections::HashSet;

/// Bir bağdaştırıcı için ayrılmış (dedicated) VRAM kullanım dökümü — bayt.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct GpuBreakdown {
    /// Tüm süreçlerin toplamı (Görev Yöneticisi'ndeki "Dedicated GPU memory" ile aynı).
    pub device: u64,
    /// Dragnet süreç ağacı (bu süreç + alt süreçler: WebView2 vb.).
    pub app: u64,
    /// Yalnız bu süreç (semantik motor/ORT burada çalışır).
    pub own: u64,
}

/// LUID'i sayaç örnek adı önekine çevirir: `luid_0x00000000_0x0001047E`.
pub fn luid_prefix(high: i32, low: u32) -> String {
    format!("luid_0x{:08X}_0x{:08X}", high as u32, low)
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows::core::PCWSTR;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Performance::{
        PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
        PdhOpenQueryW, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_LARGE, PDH_HCOUNTER, PDH_HQUERY,
    };

    const PDH_MORE_DATA: u32 = 0x800007D2;
    const ADAPTER_PATH: &str = r"\GPU Adapter Memory(*)\Dedicated Usage";
    const PROCESS_PATH: &str = r"\GPU Process Memory(*)\Dedicated Usage";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Bir sayacın tüm örneklerini (ad, değer) olarak okur.
    /// SAFETY: PDH tampon protokolü — önce boyut sorulur (PDH_MORE_DATA), sonra o boyutta
    /// hizalı tampon verilir; `count` kadar öğe okunur.
    unsafe fn read_array(counter: PDH_HCOUNTER) -> Vec<(String, u64)> {
        let mut size = 0u32;
        let mut count = 0u32;
        let st = PdhGetFormattedCounterArrayW(counter, PDH_FMT_LARGE, &mut size, &mut count, None);
        if st != PDH_MORE_DATA || size == 0 {
            return Vec::new();
        }
        let item_sz = std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        let cap = (size as usize).div_ceil(item_sz) + 1;
        let mut buf: Vec<PDH_FMT_COUNTERVALUE_ITEM_W> = Vec::with_capacity(cap);
        let st = PdhGetFormattedCounterArrayW(
            counter,
            PDH_FMT_LARGE,
            &mut size,
            &mut count,
            Some(buf.as_mut_ptr()),
        );
        if st != 0 {
            return Vec::new();
        }
        buf.set_len(count as usize);
        buf.iter()
            .map(|it| {
                let name = it.szName.to_string().unwrap_or_default();
                let val = it.FmtValue.Anonymous.largeValue.max(0) as u64;
                (name, val)
            })
            .collect()
    }

    /// Bu süreç + tüm alt süreçleri (özyinelemeli). Tauri arayüzü ayrı WebView2
    /// süreçlerinde çalıştığı için GPU belleğinin bir kısmı orada görünür.
    pub fn process_tree() -> HashSet<u32> {
        let me = std::process::id();
        let mut out = HashSet::from([me]);
        // SAFETY: Toolhelp anlık görüntüsü; tutamak kapsam sonunda düşer (Owned handle).
        unsafe {
            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return out;
            };
            let mut e = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut pairs: Vec<(u32, u32)> = Vec::new(); // (pid, ppid)
            if Process32FirstW(snap, &mut e).is_ok() {
                loop {
                    pairs.push((e.th32ProcessID, e.th32ParentProcessID));
                    if Process32NextW(snap, &mut e).is_err() {
                        break;
                    }
                }
            }
            // Ağacı kapanışa kadar genişlet (torun süreçler için birkaç tur yeter).
            let mut grew = true;
            while grew {
                grew = false;
                for (pid, ppid) in &pairs {
                    if out.contains(ppid) && out.insert(*pid) {
                        grew = true;
                    }
                }
            }
        }
        out
    }

    /// İki sayacı tek sorguda toplayıp bağdaştırıcıya göre süzer.
    pub fn breakdown(luid_prefix: &str) -> Option<GpuBreakdown> {
        let pids = process_tree();
        // SAFETY: PDH sorgusu açılır, sayaçlar eklenir, tek toplama yapılır, sorgu kapatılır.
        unsafe {
            let mut query = PDH_HQUERY::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }
            let add = |path: &str| -> Option<PDH_HCOUNTER> {
                let w = wide(path);
                let mut c = PDH_HCOUNTER::default();
                (PdhAddEnglishCounterW(query, PCWSTR(w.as_ptr()), 0, &mut c) == 0).then_some(c)
            };
            let adapter = add(ADAPTER_PATH);
            let process = add(PROCESS_PATH);
            if adapter.is_none() && process.is_none() {
                PdhCloseQuery(query);
                return None;
            }
            if PdhCollectQueryData(query) != 0 {
                PdhCloseQuery(query);
                return None;
            }
            let mut out = GpuBreakdown::default();
            if let Some(c) = adapter {
                for (name, val) in read_array(c) {
                    if name.starts_with(luid_prefix) {
                        out.device += val;
                    }
                }
            }
            if let Some(c) = process {
                let me = std::process::id();
                for (name, val) in read_array(c) {
                    // Örnek adı: pid_1234_luid_0x…_phys_0
                    let Some(rest) = name.strip_prefix("pid_") else {
                        continue;
                    };
                    let Some((pid_s, tail)) = rest.split_once('_') else {
                        continue;
                    };
                    if !tail.starts_with(luid_prefix) {
                        continue;
                    }
                    let Ok(pid) = pid_s.parse::<u32>() else {
                        continue;
                    };
                    if pids.contains(&pid) {
                        out.app += val;
                        if pid == me {
                            out.own += val;
                        }
                    }
                }
            }
            PdhCloseQuery(query);
            Some(out)
        }
    }
}

#[cfg(windows)]
pub use imp::breakdown;

/// Windows dışında sayaç yok.
#[cfg(not(windows))]
pub fn breakdown(_luid_prefix: &str) -> Option<GpuBreakdown> {
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn luid_prefix_bicimi() {
        assert_eq!(
            super::luid_prefix(0, 0x0001_047E),
            "luid_0x00000000_0x0001047E"
        );
    }

    #[cfg(windows)]
    #[test]
    fn sayac_okumasi_cokmemeli() {
        // Gerçek bir LUID gerekmez: eşleşme olmazsa sıfır döner, çökmemeli.
        let _ = super::breakdown("luid_0x00000000_0x00000000");
    }
}

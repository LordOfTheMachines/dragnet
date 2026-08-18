// SPDX-License-Identifier: AGPL-3.0-only
//! Donanım keşfi: GPU video belleği (DXGI, doğrudan/okuma-yalnız) ve kademe önerisi.
//!
//! `nvidia-smi` gibi harici araçlar yerine DXGI `IDXGIAdapter3::QueryVideoMemoryInfo`
//! kullanılır: işletim sisteminin bu sürece verdiği **bütçe** ve sürecin **anlık kullanımı**
//! (bayt). Süreç-yerel bir sorgudur, GPU'yu meşgul etmez. Yalnız Windows; diğer
//! platformlarda `None`.

/// Bir GPU bağdaştırıcısının bellek durumu (bayt).
#[derive(Debug, Clone, serde::Serialize)]
pub struct GpuMemory {
    pub adapter: String,
    /// İşletim sisteminin bu sürece verdiği yerel video belleği bütçesi.
    pub budget: u64,
    /// Bu sürecin anlık yerel video belleği kullanımı.
    pub current_usage: u64,
    /// Bağdaştırıcının toplam ayrılmış video belleği (VRAM).
    pub dedicated_total: u64,
}

/// İlk (birincil) donanım GPU'sunun bellek bilgisi. Yazılım/temel bağdaştırıcılar atlanır.
#[cfg(windows)]
pub fn gpu_memory() -> Option<GpuMemory> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter1, IDXGIAdapter3, IDXGIFactory1,
        DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO,
    };
    // SAFETY: DXGI COM çağrıları; dönüş değerleri kontrol edilir, ham işaretçi yok.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut i = 0u32;
        loop {
            let adapter: IDXGIAdapter1 = match factory.EnumAdapters1(i) {
                Ok(a) => a,
                Err(_) => break,
            };
            i += 1;
            let desc = adapter.GetDesc1().ok()?;
            if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
                continue;
            }
            let name = String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .to_string();
            let a3: IDXGIAdapter3 = adapter.cast().ok()?;
            let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            if a3
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info)
                .is_err()
            {
                continue;
            }
            return Some(GpuMemory {
                adapter: name,
                budget: info.Budget,
                current_usage: info.CurrentUsage,
                dedicated_total: desc.DedicatedVideoMemory as u64,
            });
        }
        None
    }
}

#[cfg(not(windows))]
pub fn gpu_memory() -> Option<GpuMemory> {
    None
}

/// Donanıma göre kademe önerisi (kullanıcı "otomatik" seçtiğinde):
/// - Ayrılmış VRAM ≥ 1.5 GB olan bir GPU varsa → `Quality` (Gemma; DirectML'de 2–3× hızlı indeksleme).
/// - GPU yok ama ≥ 8 mantıksal çekirdek → `Quality` yine (CPU'da ~45 ad/sn; kalite öncelikli).
/// - 4–7 çekirdek → `Balanced` (MiniLM, ~380 ad/sn).
/// - Daha azı → `Light` (potion, anında).
///
/// Bake-off (ARCHITECTURE §7.3): kalite farkı büyük (Gemma 0.93 vs MiniLM 0.70 vs potion 0.64),
/// bu yüzden yalnız zayıf makinelerde düşülür.
pub fn recommend_tier() -> (crate::Tier, String) {
    let cores = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(2);
    if let Some(g) = gpu_memory() {
        if g.dedicated_total >= 1_500 * 1024 * 1024 {
            return (
                crate::Tier::Quality,
                format!(
                    "GPU: {} ({} MB VRAM) → yüksek kalite (DirectML)",
                    g.adapter,
                    g.dedicated_total / 1_048_576
                ),
            );
        }
    }
    if cores >= 8 {
        (
            crate::Tier::Quality,
            format!("{cores} çekirdek, ayrılmış GPU yok → yüksek kalite (CPU)"),
        )
    } else if cores >= 4 {
        (
            crate::Tier::Balanced,
            format!("{cores} çekirdek → dengeli (MiniLM)"),
        )
    } else {
        (
            crate::Tier::Light,
            format!("{cores} çekirdek → hafif (potion)"),
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn recommend_returns_a_tier() {
        let (t, why) = super::recommend_tier();
        assert!(!why.is_empty());
        let _ = t;
        // GPU sorgusu çökmemeli (Windows'ta bir değer, diğerlerinde None).
        let _ = super::gpu_memory();
    }
}

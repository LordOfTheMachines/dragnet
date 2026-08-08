<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# Dragnet — Lisanslama

Dragnet **çift lisanslıdır (dual licensing).** Kullanıcı iki seçenekten birini seçer.

## 1. Açık kaynak seçeneği — AGPL-3.0-only

Varsayılan lisans **GNU Affero General Public License v3.0**'dır (`LICENSE` dosyası).

Neden AGPL (basit MIT/GPL değil):
- **Copyleft:** Dragnet'ten türeyen her çalışma da AGPL altında ve **kaynak koduyla birlikte**
  dağıtılmak zorundadır.
- **Ağ hükmü (AGPL'i özel kılan madde):** Biri Dragnet'i (veya türevini) bir **ağ servisi**
  olarak çalıştırırsa — ki bir arama API'si tam da budur — o servisin kullanıcılarına
  değiştirilmiş kaynağı sunmak zorundadır. Sıradan GPL bunu kapsamaz; AGPL kapsar.
- Bu, "kodu kullanan onu açık yayınlamak zorunda olsun" hedefinin doğru hukuki aracıdır.

## 2. Ticari seçenek — Ayrı ticari lisans

AGPL'in kaynak-açıklama yükümlülüklerine uymak istemeyen (kodunu kapalı tutmak isteyen)
ticari kullanıcılar, telif hakkı sahibinden **ücretli bir ticari lisans** alır.
Şablon ve politika: `COMMERCIAL-LICENSE.md`.

Bu, Qt ve MongoDB gibi projelerin kullandığı kanıtlanmış modeldir:
- Topluluk ve açık projeler AGPL ile ücretsiz kullanır.
- Kodunu açmak istemeyen şirketler lisans bedeli öder.

## 3. Katkı ve telif hakkı (önemli ön koşul)

Çift lisans satabilmek için **tüm telif haklarını lisanslama yetkisine** sahip olmalısın.
Dışarıdan katkı kabul edeceksen bir **CLA (Contributor License Agreement)** veya DCO gerekir;
aksi halde katkıcıların kodunu ticari lisansla satamazsın. Bu, ilk dış katkıdan önce kurulmalı.
(Faz 7 kapsamında ele alınacak; tek geliştirici olduğun sürece sorun değil.)

## 4. Kaynak dosya başlıkları

Her kaynak dosyanın en üstüne SPDX satırı eklenir:

```
// SPDX-License-Identifier: AGPL-3.0-only      (Rust)
# SPDX-License-Identifier: AGPL-3.0-only       (Python)
<!-- SPDX-License-Identifier: AGPL-3.0-only -->(Markdown)
```

## 5. Yapılacaklar

- [x] `LICENSE` dosyasına AGPLv3'ün **tam kanonik metnini** yerleştir
      (kısa telif/çift-lisans önsözü + birebir kanonik AGPL-3.0 metni).
- [x] Telif hakkı sahibi: **Mehmet Gilik (LordOfTheMachines)**, yıl 2026 — `LICENSE` ve
      `Cargo.toml` `authors` alanında netleştirildi.
- [ ] `COMMERCIAL-LICENSE.md` fiyat/kapsam politikasını doldur (iletişim kanalı eklendi).

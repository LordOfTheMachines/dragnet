<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# Dragnet — Sürüm ve Oto-Güncelleme Akışı

`dragnet-app` masaüstü uygulaması, **GitHub Releases** üzerinden dağıtılır ve
**ed25519 imzalı** oto-güncelleme kullanır (Sello deseni, ama bağımsız/AGPL).
Yayınlanan `dragnet-app.exe`'yi kullananlar bu kanaldan güncellenir; kendisi
derlemek isteyenler `cargo build` ile istediği gibi çalışır.

## Anahtarlar (bir kez)

Özel imza tohumu **repo dışında** saklanır: `%USERPROFILE%\.dragnet-updater\ed25519_seed.bin`.
Public key uygulamaya gömülüdür: `bin/dragnet-app/src/updater.rs` → `PUBKEY_B64`.

İlk kurulumda anahtar üret:

```bash
cargo run -p dragnet-sign -- generate
# Çıktıdaki PUBKEY_B64'ü updater.rs'e göm (zaten gömülü).
```

Mevcut tohumdan public key'i tekrar görmek için: `cargo run -p dragnet-sign -- pubkey`.

> ⚠️ `ed25519_seed.bin`'i kaybetme ve **asla repoya koyma**. Kaybolursa yeni anahtar
> üretip yeni `PUBKEY_B64`'ü gömmen ve o sürümden itibaren yeniden imzalaman gerekir
> (eski istemciler yeni imzayı doğrulayamaz).

## Sürüm çıkarma

1. **Sürümü yükselt** (ikisi senkron olmalı):
   - `Cargo.toml` → `[workspace.package] version`
   - `bin/dragnet-app/tauri.conf.json` → `version`

2. **Derle** (release, taşınabilir tek exe):
   ```bash
   cargo build --release -p dragnet-app
   # → target/release/dragnet-app.exe
   ```

3. **İmzala** (ed25519 `.sig` üretir):
   ```bash
   cargo run -q -p dragnet-sign -- sign target/release/dragnet-app.exe
   # → target/release/dragnet-app.exe.sig
   ```

4. **GitHub Release oluştur** — varlıklar **tam bu adlarla** yüklenmeli:
   ```bash
   gh release create vX.Y.Z --latest --title "Dragnet vX.Y.Z" --notes "…" \
     target/release/dragnet-app.exe \
     target/release/dragnet-app.exe.sig
   ```
   Etiket `vX.Y.Z` biçiminde olmalı (updater `tag_name`'i karşılaştırır).

## Oto-güncelleme nasıl çalışır (updater.rs)

- Uygulama, `https://api.github.com/repos/LordOfTheMachines/dragnet/releases/latest`'i
  sorgular; `tag_name` mevcut sürümden yeniyse `dragnet-app.exe` ve `.sig` varlıklarını indirir.
- İmza, gömülü `PUBKEY_B64` ile **strict** doğrulanır; doğrulanamazsa güncelleme reddedilir.
- Doğrulanan exe, çalışan exe'nin yerine konur (`self-replace`) ve uygulama yeniden başlatılır.
- Repo kod içinde sabit (`CANONICAL_REPO`) — başka bir repodan güncelleme kabul edilmez.

## Güvenlik notları

- Telemetri, uzaktan takip, lisans-kill YOK — yalnız imzalı oto-güncelleme.
- Özel anahtar yalnız yayıncının makinesinde; CI'da imzalanacaksa `DRAGNET_SEED_B64`
  ortam değişkeni (base64 tohum) gizli olarak verilebilir (`dragnet-sign` bunu okur).

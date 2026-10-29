<div align="center">

<img src="docs/icon.png" width="96" alt="Ikon LOKA: Oka, kursor berwajah, di samping tepi layar yang menyala">

# LOKA

**Satu mouse dan keyboard untuk semua komputer di mejamu.**
Dorong kursor melewati tepi layar yang menyala, dan kursor langsung pindah ke perangkat di sebelahnya.

[English](README.md) · **Bahasa Indonesia** · [Website](https://loka.margingroup.id) · [Unduh](https://github.com/fanrsd/LOKA/releases/latest)

<img src="docs/screenshots/calibrate.webp" width="640" alt="LOKA terhubung dengan Studio PC, tepi menyala di sisi kanan layar 1">

</div>

## Fitur

- **Berbagi mouse dan keyboard.** Satu komputer menjadi **Main**: mouse dan keyboard-nya mengendalikan perangkat lain. Setiap perangkat di sekitarnya menjalankan LOKA sebagai **Client**.
- **Bezel Portal.** Tandai bagian tepi layar tempat perangkat lain berada di mejamu. Dorong kursor ke garis yang menyala itu, dan kursor pindah ke sana. Dorong keluar dari sisi seberang perangkat itu untuk kembali. **Ctrl+Alt+Esc** selalu mengembalikannya.
- **Keyboard dan clipboard ikut kursor.** Ketikan selalu masuk ke perangkat tempat kursor berada. Salin teks di satu komputer, pindah, lalu tempel di komputer lain (hanya teks, dua arah).
- **Lempar file ke tepi layar.** Seret file ke garis yang menyala lalu lepaskan. File masuk ke `Downloads/LOKA` di perangkat pasangan. File lama tidak pernah ditimpa.
 Garis yang menyala terisi seiring file berjalan, dan kedua komputer menampilkan progres, kecepatan, serta sisa waktunya. Transfer bisa dibatalkan, foldernya bisa langsung dibuka saat file tiba, dan ada notifikasi sistem ketika file masuk.
- **Pairing ala LocalSend.** Perangkat di jaringan yang sama saling menemukan. Klik salah satunya, lalu konfirmasi di perangkat itu. Di jaringan yang memblokir pencarian perangkat (Wi-Fi tamu), tambahkan Client lewat alamat IP-nya.
- **Selalu terbaru.** Menu **About** menampilkan versi, isi tiap rilis, dan memberi tahu jika ada versi baru di GitHub.
- **Sesuaikan sendiri.** Pilih warna glow (sekaligus jadi warna aksen aplikasi), seberapa transparan glow-nya, dan animasi saat menyeberang: Ripple, Pulse, atau Calm.
- **Tinggal di tray.** Menutup jendela tidak menghentikan LOKA; berbagi tetap jalan di latar belakang. Klik ikon di tray untuk membukanya lagi, atau pilih **Quit LOKA** di sana untuk berhenti.

Kenalan dengan **Oka**, si kecil dari LOKA: sebuah kursor, benda yang memang berpindah antar layar. Setiap perangkat punya warna Oka sendiri, dan wajahnya menunjukkan status: mencari, menunggu, terhubung, atau offline.

| | |
|---|---|
| <img src="docs/screenshots/home.webp" alt="Perangkat di sekitar, masing-masing tampil sebagai Oka"> | <img src="docs/screenshots/pairing.webp" alt="Permintaan pairing dengan dua Oka saling berhadapan"> |
| <img src="docs/screenshots/client-waiting.webp" alt="Client menunggu Main"> | <img src="docs/screenshots/glow-violet.webp" alt="Layar yang sama dengan glow ungu"> |

## Platform

| Platform | Status |
|---|---|
| Windows 10/11 | Berjalan (Main dan Client) |
| macOS | Kode lengkap, masih diuji di perangkat asli. Perlu izin **Accessibility**. |
| Android | Direncanakan |
| iOS | Tidak direncanakan: iOS tidak mengizinkan aplikasi mengendalikan input perangkat |

## Instalasi

Unduh installer dari [Releases](https://github.com/fanrsd/LOKA/releases/latest):

- **Windows:** `LOKA_x.y.z_x64-setup.exe` atau `.msi`. Jika SmartScreen memperingatkan, pilih **More info → Run anyway**. Saat Windows Firewall bertanya, centang **Private** dan **Public**. Perangkat masih tidak saling terlihat? Di LOKA buka **About → Allow through Windows Firewall**.
- **macOS:** `LOKA_x.y.z_universal.dmg` (Apple Silicon dan Intel). Buka, seret LOKA ke **Applications**, lalu buka dari folder Applications. LOKA belum dinotarisasi, jadi pembukaan pertama diblokir: klik **Done** (jangan Move to Trash), lalu:
  - macOS 15 ke atas: System Settings → Privacy & Security → gulir ke bawah → **Open Anyway**.
  - macOS 14 ke bawah: klik kanan LOKA di Applications → **Open** → **Open**.
  - "LOKA is damaged and can't be opened" (atau versi apa pun): jalankan di Terminal

    ```bash
    xattr -dr com.apple.quarantine /Applications/LOKA.app
    ```

  Izinkan **Accessibility** saat diminta (System Settings → Privacy & Security → Accessibility), di Main maupun Client; di macOS 15+ izinkan juga **Local Network**. Setelah update, jika mouse tidak bisa pindah lagi, hapus LOKA dari daftar Accessibility dengan tombol **−** lalu aktifkan lagi.

Lalu:

1. Buka LOKA di kedua komputer dan pilih peran: **Main** untuk komputer yang punya mouse dan keyboard, **Client** untuk yang lain.
2. Di Main, klik Oka milik Client. Terima di Client.
3. Pilih tepi layar, lalu geser bar yang menyala sampai sejajar dengan letak perangkat lain.

Kedua perangkat harus berada di jaringan lokal yang sama. LOKA memakai UDP 53318 (pencarian perangkat) dan TCP 53319–53321 (telemetri, file, input). Wi-Fi tamu atau router dengan client isolation memblokir pencarian: di Client, LOKA menampilkan **This computer's address**; ketik alamat itu di **Add by IP** pada Main.

## Build dari source

Kebutuhan: [Node.js](https://nodejs.org) 20+, [Rust](https://rustup.rs) (stable), dan [prasyarat Tauri](https://tauri.app/start/prerequisites/) untuk OS-mu.

```bash
npm install
npm run tauri dev        # jalankan
npm run sim              # jalankan dengan simulasi Client di ponsel
npm run tauri build      # installer ada di src-tauri/target/release/bundle
```

## Keamanan

LOKA dibuat untuk jaringan rumah atau kantor yang tepercaya. Hanya perangkat yang sudah di-pairing yang bisa mengirim input atau file, tetapi lalu lintasnya belum dienkripsi. Hindari Wi-Fi publik.

## Lisensi

[Apache License 2.0](LICENSE). Fork dan versi yang diubah wajib menyertakan file [NOTICE](NOTICE) dan mencantumkan proyek aslinya:

> Based on LOKA by Aufa Nirsad: https://github.com/fanrsd/LOKA

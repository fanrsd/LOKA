<div align="center">

<img src="docs/icon.png" width="96" alt="Ikon LOKA: Oka, kursor berwajah, di samping tepi layar yang menyala">

# LOKA

**Satu mouse dan keyboard untuk semua komputer di mejamu.**
Dorong kursor melewati tepi layar yang menyala, dan kursor langsung pindah ke perangkat di sebelahnya.

[English](README.md) · **Bahasa Indonesia** · [Website](https://loka.margingroup.id) · [Unduh](https://github.com/fanrsd/LOKA/releases/latest)

<img src="docs/screenshots/calibrate.webp" width="640" alt="LOKA terhubung dengan Studio PC, tepi menyala di sisi kanan layar 1">

</div>

## Fitur

- **Berbagi mouse dan keyboard, ke arah mana pun.** Setiap komputer yang menjalankan LOKA bisa mengendalikan yang lain dan dikendalikan olehnya: dorong kursor dari komputer mana pun yang sedang kamu pegang.
- **Bezel Portal.** Tandai bagian tepi layar tempat perangkat lain berada di mejamu. Dorong kursor ke garis yang menyala itu, dan kursor pindah ke sana. Dorong keluar dari sisi seberang perangkat itu untuk kembali. **Ctrl+Alt+Esc** atau tekan **Ctrl** dua kali selalu mengembalikannya. Taruh beberapa perangkat di tepi berbeda, dan kursor bisa terus lanjut dari satu ke berikutnya (PC → Mac → laptop).
- **Terasa seperti satu komputer.** Kursor menghilang dari layar komputer yang sedang mengendalikan. Ctrl berfungsi sebagai ⌘ di Mac (dan ⌘ sebagai Ctrl di PC), jadi shortcut tetap jalan. Atur kecepatan pointer di seberang dan jeda tepi opsional supaya kursor tidak menyeberang saat lewat sekilas. **Scroll Lock** (F14 di Mac) mengunci pointer di layar tempat dia berada. Swipe trackpad Mac antar-desktop, Mission Control, dan pinch-to-zoom juga jalan di komputer lain.
- **Keyboard dan clipboard ikut kursor.** Ketikan selalu masuk ke perangkat tempat kursor berada. Salin di satu komputer, pindah, lalu tempel di komputer lain, dua arah: teks, gambar (termasuk screenshot, sampai 10 MB), dan file yang kamu salin di Explorer atau Finder. Isi yang ditandai rahasia oleh password manager tidak ikut dikirim. **Recent clipboard** menyimpan 12 hal terakhir yang kamu salin, gambar juga: klik untuk menyalinnya lagi.
- **Seret file menyeberang.** Ambil file di Explorer atau Finder lalu terus seret melewati tepi yang menyala: kursor membawanya ke seberang, dan kamu bisa melepasnya di folder atau aplikasi mana pun di komputer lain. File besar mengalir: drop langsung dimulai dan terisi seiring file tiba.
- **Lempar file ke tepi layar, dua arah.** Seret file atau folder ke garis yang menyala lalu lepaskan. Semuanya masuk ke `Downloads/LOKA` (atau folder pilihanmu) di perangkat pasangan, folder lengkap dengan isinya, beberapa file sekaligus. Saat kamu sedang mengendalikan komputer lain, tepinya yang mengarah balik juga menyala: seret file ke sana untuk mengirim ke arah sebaliknya. File lama tidak pernah ditimpa.
 Garis yang menyala terisi seiring file berjalan, dan kedua komputer menampilkan progres, kecepatan, serta sisa waktunya. Transfer bisa dibatalkan, foldernya bisa langsung dibuka saat file tiba, dan ada notifikasi sistem ketika file masuk. **Ctrl+Alt+V** menempel file yang baru tiba ke aplikasi di depan (chat, email). Bisa juga kirim dari **Send to → LOKA** di Explorer atau **Open With → LOKA** di Finder.
- **Pairing ala LocalSend.** Perangkat di jaringan yang sama saling menemukan. Klik salah satunya, lalu konfirmasi di perangkat itu. Di jaringan yang memblokir pencarian perangkat (Wi-Fi tamu), tambahkan komputer lain lewat alamat IP-nya. Saat pairing, kedua layar menampilkan kode 6 digit yang sama, jadi kamu tahu perangkatnya benar, dan tidak ada orang di jaringan yang bisa memalsukannya.
- **Selalu terbaru.** Menu **About** menampilkan versi, isi tiap rilis, dan memberi tahu jika ada versi baru di GitHub.
- **Sesuaikan sendiri.** Pilih warna glow (sekaligus jadi warna aksen aplikasi), seberapa transparan glow-nya, dan animasi saat menyeberang: Ripple, Pulse, atau Calm. Setiap klik di LOKA berbunyi "tik" pelan; ikon speaker di pojok kanan atas mematikannya.
- **Berbagi lebih, hanya kalau kamu mau.** Setiap komputer bisa berbagi folder (terbuka sebagai drive di komputer lain), suaranya (diputar lewat speaker komputer lain), notifikasinya (Windows), mikrofonnya (dipakai sebagai mic untuk call di komputer lain, lewat VB-CABLE atau BlackHole yang gratis), dan terminal. Semuanya mati sampai kamu nyalakan.
- **Koneksi yang tahu diri.** Kalau komputer lain tidur, lepas dari Wi-Fi, atau macet, kursor pulang dalam sekitar 3 detik dan tidak ada tombol yang tertahan di sana. Pindah Wi-Fi di tengah sesi tetap jalan.
- **Bangunkan dari jauh.** Komputer pasangan yang sedang tidur punya tombol **Wake up** (Wake-on-LAN).
- **Tinggal di tray.** Menutup jendela tidak menghentikan LOKA. Menu tray bisa **Control** perangkat mana pun tanpa menyentuh tepi, **Disconnect**, membuka LOKA lagi, atau **Quit LOKA**.

Kenalan dengan **Oka**, si kecil dari LOKA: sebuah kursor, benda yang memang berpindah antar layar. Setiap perangkat punya warna Oka sendiri, dan wajahnya menunjukkan status: mencari, menunggu, terhubung, atau offline.

| | |
|---|---|
| <img src="docs/screenshots/home.webp" alt="Perangkat di sekitar, masing-masing tampil sebagai Oka"> | <img src="docs/screenshots/pairing.webp" alt="Permintaan pairing dengan dua Oka saling berhadapan"> |
| <img src="docs/screenshots/waiting.webp" alt="Menunggu MacBook Air menerima, dengan kode pairing untuk dicocokkan"> | <img src="docs/screenshots/glow-violet.webp" alt="Layar yang sama dengan glow ungu"> |

## Platform

| Platform | Status |
|---|---|
| Windows 10/11 | Berjalan |
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

  Izinkan **Accessibility** saat diminta (System Settings → Privacy & Security → Accessibility), di setiap Mac; di macOS 15+ izinkan juga **Local Network**. Setelah update, jika mouse tidak bisa pindah lagi, hapus LOKA dari daftar Accessibility dengan tombol **−** lalu aktifkan lagi.

Lalu:

1. Buka LOKA di kedua komputer.
2. Di salah satunya, klik Oka komputer lain. Pastikan kedua layar menampilkan kode yang sama, lalu terima di komputer lain.
3. Pilih tepi layar, lalu geser bar yang menyala sampai sejajar dengan letak perangkat lain.

Kedua perangkat harus berada di jaringan lokal yang sama. LOKA memakai UDP 53318 (pencarian perangkat) dan TCP 53319–53321 (telemetri, file, input). Wi-Fi tamu atau router dengan client isolation memblokir pencarian: LOKA menampilkan **This computer's address** di bawah **Add by IP**; ketik alamat itu di **Add by IP** pada komputer lain.

## Build dari source

Kebutuhan: [Node.js](https://nodejs.org) 20+, [Rust](https://rustup.rs) (stable), dan [prasyarat Tauri](https://tauri.app/start/prerequisites/) untuk OS-mu.

```bash
npm install
npm run tauri dev        # jalankan
npm run sim              # jalankan dengan simulasi ponsel
npm run tauri build      # installer ada di src-tauri/target/release/bundle
```

## Keamanan

LOKA dibuat untuk jaringan rumah atau kantor. Setiap koneksi antarperangkat dienkripsi (TLS 1.3) dan setiap perangkat punya sertifikat sendiri: setelah di-pairing, hanya perangkat itu yang bisa mengirim input atau file, meskipun ada yang meniru nama atau alamatnya di jaringan.

## Lisensi

[Apache License 2.0](LICENSE). Fork dan versi yang diubah wajib menyertakan file [NOTICE](NOTICE) dan mencantumkan proyek aslinya:

> Based on LOKA by Aufa Nirsad: https://github.com/fanrsd/LOKA

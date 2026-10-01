# Panduan penggunaan ProgramCall

ProgramCall asset digunakan melalui wallet CLI dengan perintah `program-*`.
CLI membuat payload, menandatangani call beserta pembayaran XPQ, menghitung
biaya, dan mengirim transaksi ke node. GUI belum diperlukan untuk alur ini.
Asset hanya menggunakan registry extension Program; perintah legacy `asset-*`
sudah dihapus.

## Persiapan

Gunakan node dengan chain/storage yang kompatibel: chain spec 1 dan storage
schema 6. Database lama tidak dimigrasikan otomatis; lihat
[status integrasi](PROGRAM_CALL.md) untuk batas kompatibilitas.

Node dan RPC harus berjalan. Wallet pengirim harus mempunyai XPQ yang dapat
dibelanjakan untuk miner fee dan protocol burn, termasuk pada transfer asset.
Kernel menyimpan coin XPQ pada `UtxoSet`, sedangkan record dan share asset
disimpan pada `LedgerState.extensions`. Asset baru harus didaftarkan ulang pada
chain setelah reset; saldo asset legacy tidak dipindahkan otomatis.

```bash
cargo build -p wallet --release

XPA_WALLET=./wallet.json
XPA_RPC=127.0.0.1:6666
```

Jika belum memiliki wallet, buat wallet lalu lihat alamatnya:

```bash
./target/release/wallet new --wallet "$XPA_WALLET"
./target/release/wallet address --wallet "$XPA_WALLET"
```

Simpan informasi pemulihan wallet. Isi saldo XPQ sebelum membuat transaksi
Program. Semua contoh berikut menggunakan shell Bash.

## 1. Daftarkan asset

Contoh: GOLD memiliki batas mint 100 unit dan mint awal 40 unit ke wallet pembuat.

```bash
./target/release/wallet program-register \
  --wallet "$XPA_WALLET" --rpc "$XPA_RPC" \
  --name GOLD --max-supply 100 --initial-mint 40
```

Simpan ID pada keluaran `program asset: ...`:

```bash
XPA_ASSET='ID_ASSET_DARI_OUTPUT'
XPA_RECIPIENT='ALAMAT_PENERIMA'
```

Tunggu transaksi masuk blok sebelum menjalankan operasi yang bergantung padanya.
ID yang dicetak bukan bukti bahwa pendaftaran sudah dikonfirmasi.

Secara default wallet pembuat menjadi otoritas mint. Tambahkan `--fixed-supply`
saat pendaftaran untuk menonaktifkan seluruh mint tambahan; pilih mint awal
sesuai supply yang diinginkan.

## 2. Periksa metadata dan saldo

```bash
./target/release/wallet program-info \
  --rpc "$XPA_RPC" --asset "$XPA_ASSET"

./target/release/wallet program-balance \
  --wallet "$XPA_WALLET" --rpc "$XPA_RPC" --asset "$XPA_ASSET"

./target/release/wallet program-balance \
  --rpc "$XPA_RPC" --asset "$XPA_ASSET" --address "$XPA_RECIPIENT"
```

Metadata memuat supply, total minted, total burned, dan nonce mint. Respons
Program info/balance menggunakan raw units: `1500000000` berarti 15 unit asset.
Input jumlah pada perintah transaksi memakai angka desimal, maksimal 8 angka
di belakang titik; misalnya `--amount 1.25` berarti 1,25 unit.

## 3. Mint tambahan

Jalankan menggunakan wallet pemegang otoritas mint:

```bash
./target/release/wallet program-mint \
  --wallet "$XPA_WALLET" --rpc "$XPA_RPC" \
  --asset "$XPA_ASSET" --to "$XPA_RECIPIENT" --amount 20
```

CLI mengambil nonce berikutnya dari node. Mint tetap tunduk pada batas mint
asset dan ditolak jika otoritas mint dinonaktifkan.

## 4. Transfer

```bash
./target/release/wallet program-transfer \
  --wallet "$XPA_WALLET" --rpc "$XPA_RPC" \
  --asset "$XPA_ASSET" --to "$XPA_RECIPIENT" --amount 15
```

CLI memilih share milik pengirim dan mengembalikan sisa asset ke pengirim.
Penerima tidak perlu menerima XPQ bersama asset. Untuk membelanjakan asset
tersebut nanti, penerima membutuhkan XPQ untuk biaya transaksinya sendiri.

## 5. Burn

```bash
./target/release/wallet program-burn \
  --wallet "$XPA_WALLET" --rpc "$XPA_RPC" \
  --asset "$XPA_ASSET" --amount 5
```

Burn mengurangi saldo asset milik wallet dan supply beredar. Sisa asset dari
share yang dipilih dikembalikan ke wallet. Burn asset dan protocol burn XPQ
merupakan dua komponen yang berbeda.

## 6. Konsolidasi share

```bash
./target/release/wallet program-consolidate \
  --wallet "$XPA_WALLET" --rpc "$XPA_RPC" --asset "$XPA_ASSET"
```

Konsolidasi menggabungkan maksimal 256 share menjadi satu share pada alamat
wallet sendiri dan membutuhkan minimal dua share. Jika masih banyak share,
tunggu konfirmasi sebelum menjalankannya kembali. Batas 256 share berlaku per
call, bukan untuk seluruh saldo atau sepanjang umur asset.

## Transaksi tanpa pengiriman langsung

Tambahkan `--offline` untuk mencetak `Transaction Hex` tanpa submit:

```bash
./target/release/wallet program-transfer \
  --wallet "$XPA_WALLET" --rpc "$XPA_RPC" \
  --asset "$XPA_ASSET" --to "$XPA_RECIPIENT" --amount 1 --offline
```

Mode ini tetap membutuhkan RPC untuk memilih input dan mendapatkan quote
state growth. Hex yang dicetak sudah ditandatangani; validitasnya bergantung
pada input dan state saat transaksi akhirnya dikirim.

## Biaya dan urutan operasi

Batas berikut berlaku untuk satu ProgramCall, bukan sepanjang umur asset atau
untuk seluruh blok:

| Operasi | Maksimum payload | Batas share/output |
| --- | --- | --- |
| Register | 4 KiB | Mint awal ke pembuat |
| Mint | 1 KiB | Satu penerima |
| Transfer | 64 KiB | 256 input share dan 256 output |
| Burn | 32 KiB | 256 input share |

Envelope ProgramCall membatasi payload secara global hingga 64 KiB. Batas
transaksi 256 KiB dan batas blok tetap berlaku secara terpisah. Konsolidasi
menggunakan opcode Transfer dengan satu output ke wallet sendiri.

Biaya dihitung otomatis berdasarkan transaksi bertanda tangan dan preview
state dari `POST /program/quote`. Jangan memberikan biaya manual.
Quote tidak memasukkan transaksi ke mempool dan tidak mengubah state.

Tunggu konfirmasi sebelum operasi lanjutan yang memakai share atau nonce yang
sama. Jika input sudah terpakai atau nonce berubah, jalankan ulang perintah
untuk membuat transaksi berdasarkan state terbaru.

Untuk error saldo asset tidak cukup dalam 256 share, periksa saldo dan transaksi
pending, lalu konsolidasikan share bila diperlukan. Untuk error saldo XPQ,
isi saldo XPQ yang dapat dibelanjakan pada wallet penandatangan.

Menu interaktif wallet juga menyediakan **Program Assets**. Perintah normal
`balance` dan `history` menampilkan holdings dan aktivitas Program asset.

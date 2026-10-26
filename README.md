# whereti

**Find any file on your PC, the moment you type.** whereti is a free, open
source file search for Windows: every name on your drives in a millisecond,
the words inside your documents, photos by where they were taken and what is
in them, and nothing ever leaves your PC.

Made by [Software Hive S.R.L.](https://whereti.com/who/), Romania.
Download, questions and privacy: **[whereti.com](https://whereti.com)**.

## What it does

- **Names, instantly.** It reads the NTFS master file table directly and
  follows the change journal, so a new file shows up within a second.
  FAT/exFAT drives are walked instead.
- **Inside documents.** Word, Excel, PowerPoint (old and new), OpenDocument,
  PDF (with OCR for scans), RTF, HTML, e-mails (`.eml`, `.msg`), Outlook data
  files (`.pst`), e-books (EPUB, FB2), XPS, plain text and about 80 kinds of
  source code, plus the files inside ZIP, 7z and RAR archives.
- **Photos.** By place (`loc:rome`, from the photo's GPS, named offline),
  by what they show (`dog`, `beach`: an optional local model), by the text in
  them (Windows' own OCR), by date taken and camera.
- **Search commands** compatible with Everything's core syntax:
  `ext:pdf`, `size:>10mb`, `dm:lastweek`, `path:`, wildcards, `|`, `!`, plus
  `content:`, `loc:`, `taken:`, `camera:`, `width:`, `artist:`, `duration:`,
  `author:` and `dupe:` (identical files, compared by content). Press `?` in
  the search box for all of them.
- **Results in a sensible order.** Exact names first, then by how much a file
  matters (your documents before program files), what you open often and the
  folders you work in lately. A word one letter off still finds it.
- **Quick actions.** Enter opens, Ctrl+Enter shows it in Explorer, F2
  renames, Delete moves it to the Recycle Bin, Ctrl+C copies the file, drag it
  into any app; from an Open or Save window, Enter takes that window to the
  result's folder.
- **8 languages**: English, Deutsch, Español, Français, Italiano, Polski,
  Português (Brasil), Română.

## Privacy

whereti works offline. The index, what it learns about what you open and where
you work, your past searches: all of it stays on your PC, and every switch in
Settings that learns something also forgets it. The only connections it makes:
one update check a day (sending only its version number, can be turned off),
and, only if you turn them on, a one-time download of the photo model and of
the search-by-meaning model from Hugging Face. No accounts, no telemetry, no
ads.

## How it is built

A Rust workspace. A Windows service (running as SYSTEM, so it can read the
master file table) keeps the index; the app talks to it over a local named
pipe that only accepts the signed-in user's own processes, and only shows
files that user may open. Untrusted documents are parsed in a separate,
memory-capped worker process (`whereti-extract`), never in the service.

| Crate | What it is |
|---|---|
| `whereti-ntfs` | Reading the master file table and the USN change journal |
| `whereti-core` | The name index: build, live updates, search, ranking |
| `whereti-query` | The search language, file kinds |
| `whereti-content` | The content index (tantivy) |
| `whereti-extract` | Text from documents, OCR, archives, mailboxes, photo and text models; the sandboxed worker |
| `whereti-places` | Where a photo was taken: GPS from EXIF, named with GeoNames, offline |
| `whereti-engine` | Everything the service does: index, journal, content, properties, duplicates |
| `whereti-service` | The Windows service |
| `whereti-ipc` | The named pipe protocol |
| `whereti-app` | The app (egui): search window, preview, settings, tray |
| `whereti-i18n` | The 8 languages |
| `whereti-net` | HTTPS downloads (WinHTTP) with SHA-256 checks |
| `whereti-cli` | `wt`: search, status and diagnostics from the command line |
| `whereti-setup` | The installer |

## Build

Windows 10 or 11, and the Rust version in `rust-toolchain.toml` (rustup picks
it up).

```
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

`scripts\package.ps1` builds the installer (`dist\whereti-setup-<version>.exe`);
with `-Release` it also signs it. `scripts\dev.ps1` runs a development build
against the installed service (swapping the service in place, with one
administrator prompt, only when it changed).

## License

[MIT](LICENSE). The bundled ONNX Runtime and the optional models keep their
own licenses (MIT for ONNX Runtime and the Xenova model exports).

# PDF Forge

A free, open-source PDF toolkit in Rust — a desktop viewer/editor and a command-line tool —
built on Google's [PDFium](https://pdfium.googlesource.com/pdfium/) (the engine inside Chrome).

| Crate | What it is |
|---|---|
| [`pdfforge-app`](crates/pdfforge-app) | Desktop app (egui): viewer with thumbnails, bookmarks, search and page tools |
| [`pdfforge-cli`](crates/pdfforge-cli) | `pdfforge` command-line tool |
| [`pdfforge-core`](crates/pdfforge-core) | Library the other two are built on |

## Features

- Fast continuous-scroll viewing, zoom / fit width / fit page, bookmarks, full-text search with
  highlighted matches, document properties, password-protected PDFs.
- Page tools: rotate, delete, reorder, extract, insert blank pages or other PDFs, merge, split —
  with undo/redo in the app.
- Export pages to PNG or JPEG.
- **Fill forms**: click a field to type, tick, or pick an option; flatten the form when done.
- **Sign**: draw, type or insert an image of your signature and click to place it. These are
  visual signatures (like signing a printout), not certificate-based digital signatures.
- **Password protection**: AES-256 encryption with optional permissions (printing, copying,
  editing, …); remove protection from files you can open.
- **Compress**: downsample and recompress images, remove unused objects; presets for screen,
  print and smallest size.

Status: early. Linux is the only tested platform so far.

## Building

Requires Rust 1.95+. PDFium is a prebuilt library loaded at runtime and is not stored in the
repository; fetch it once:

```sh
git clone https://github.com/R0han7/pdfforge.git
cd pdfforge
scripts/fetch-pdfium.sh          # downloads into vendor/pdfium/ (Linux and macOS)
cargo build --release
```

At runtime PDFium is looked up in `$PDFIUM_LIB_PATH`, next to the executable (and `../lib`),
`vendor/pdfium/lib` in the source tree, then the system library path. Prebuilt binaries for
other platforms are at [bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries/releases).

## Usage

```sh
pdfforge-app manual.pdf
```

| Shortcut | Action |
|---|---|
| `Ctrl+O` / `Ctrl+S` / `Ctrl+Shift+S` | Open / save / save as |
| `Ctrl+F` | Find |
| `Ctrl+Z` / `Ctrl+Shift+Z` | Undo / redo |
| `Ctrl+R` / `Ctrl+Shift+R` | Rotate right / left |
| `Del` | Delete selected pages |
| `Ctrl+=` / `Ctrl+-` / `Ctrl+0` / `Ctrl+1` | Zoom in / out / fit width / 100 % |
| `PgUp` / `PgDn` / `Home` / `End` | Navigate pages |

```sh
pdfforge info report.pdf
pdfforge search report.pdf "quarterly revenue"
pdfforge merge a.pdf b.pdf c.pdf -o all.pdf
pdfforge extract report.pdf "1-3,7,last" -o summary.pdf
pdfforge rotate scan.pdf even 180 --in-place
pdfforge split book.pdf --ranges "1-20;21-40;41-"
pdfforge render slides.pdf -p 1-5 --dpi 200

pdfforge fields form.pdf                                   # list fields, values and options
pdfforge fill form.pdf name="Ada Lovelace" subscribe=yes plan=pro --flatten
pdfforge sign contract.pdf --text "Ada Lovelace" -p last
pdfforge sign contract.pdf --image signature.png --rect 350,650,180,50
pdfforge protect report.pdf --user-password - --allow print   # reads the password from stdin
pdfforge unprotect report.pdf --password - --in-place
pdfforge compress scan.pdf --preset screen                 # or print, smallest, lossless
```

Page ranges are one-based: `1-3,5`, `8-` (to the end), `-4`, `last`, `odd`, `even`, `all`,
and `5-1` (reversed). Run `pdfforge --help` for every command.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

## License

Dual-licensed under the MIT License or the Apache License 2.0, at your option. PDFium is
distributed under its own BSD-style license.

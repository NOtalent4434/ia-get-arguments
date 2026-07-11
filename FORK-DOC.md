# ia-get Fork — Changes from Original

This fork is based on [ia-get](https://github.com/wimpysworld/ia-get) by Martin Wimpress.  
Everything the original program does still works. This document covers **what changed or was added**, how to use it correctly, and how to build the project from source.

---

## Fork vs original (summary)

| Area | Original | This fork |
|------|----------|-----------|
| **Config file** | None | Optional `ia-get.ini` (bandwidth cap, parallel downloads) |
| **Download speed** | Unlimited, one file at a time | Configurable global bandwidth limit (KB/s) |
| **Parallel downloads** | Always sequential | Optional multithreading (`threads` setting) |
| **Extension filters** | N/A | `*ext` include, `#ext` exclude (after URL) |
| **Filtered file layout** | N/A | Flat basename by default; optional `-k` / `-pk` |
| **Unfiltered download layout** | Full archive tree preserved | Same as original |
| **Cookies** | `--cookies` / `-b` (file or raw header) | Same as original |
| **List files** | `--list` / `-l` | Same, and respects active filters |
| **Resume & MD5 verify** | Yes | Yes (unchanged) |

---

## Command reference

| Argument | Short | Source | Description |
|----------|-------|--------|-------------|
| `URL` | — | original | Archive.org details page |
| `*extension` | — | **fork** | Include filter — only download files ending with this extension |
| `#extension` | — | **fork** | Exclude filter — skip files ending with this extension |
| `--list` | `-l` | original | List files from metadata and exit (no download) |
| `--cookies` | `-b` | original | Cookie header string or Netscape `cookies.txt` path |
| `--keep-folder-structure` | `-k` | **fork** | With filters: keep archive paths + create **full** directory tree from metadata |
| `--partial-folder-structure` | `-pk` | **fork** | With filters: keep archive paths, create **only** folders for downloaded files |
| `--folder` | — | **fork** | Alias for `-k` |
| `--partial-folder` | — | **fork** | Alias for `-pk` |

**Argument order:** put the URL first, then filters, then flags.

```shell
ia-get [OPTIONS] URL [FILTER...] [FLAGS...]
```

**Rules:**

- Filters must start with `*` (include) or `#` (exclude), e.g. `*apk`, `#jpg`.
- `-k` and `-pk` require at least one filter; they are ignored without filters and will error if used alone.
- `-k` and `-pk` cannot be combined.
- `-pk` on the command line is expanded to `--partial-folder-structure` (so it is not treated as a filter).

---

## How to use fork features

### 1. Extension filters

Filters match the **end of the archive path** (case-insensitive).  
You can combine includes and excludes.

**Include only APK files:**

```shell
ia-get https://archive.org/details/your-item *apk
```

**Download everything except JPGs and torrent sidecars:**

```shell
ia-get https://archive.org/details/your-item #jpg #torrent
```

**APK files only, but never `.xapk` if both exist** (include wins first, then exclude removes matches):

```shell
ia-get https://archive.org/details/your-item *apk *xapk #xapk
```

**Preview what would match before downloading:**

```shell
ia-get --list https://archive.org/details/your-item *ipa #torrent
```

If no files match after filtering, the program exits without downloading.

---

### 2. Folder layout with filters

Assume archive metadata contains:

```text
cover.jpg
data/readme.txt
apk/game.apk
apk/bonus.apk
torrent/item.torrent
```

and you run:

```shell
ia-get https://archive.org/details/your-item *apk
```

| Flags | Files downloaded | Local paths | Directories created |
|-------|------------------|-------------|---------------------|
| *(none)* | `apk/game.apk`, `apk/bonus.apk` | `game.apk`, `bonus.apk` (flat) | None |
| `-pk` | same | `apk/game.apk`, `apk/bonus.apk` | `apk/` only |
| `-k` | same | `apk/game.apk`, `apk/bonus.apk` | Full tree from **all** metadata paths (incl. empty parents like `data/`) |

**Partial folders (`-pk`)** — good default when you want paths but not empty sibling folders:

```shell
ia-get https://archive.org/details/your-item *ipa -pk
ia-get https://archive.org/details/your-item *ipa --partial-folder-structure
```

**Full tree (`-k`)** — when you want the complete archive directory layout recreated even if some folders end up empty after filtering:

```shell
ia-get https://archive.org/details/your-item *ipa -k
ia-get https://archive.org/details/your-item *ipa --keep-folder-structure
```

**Without filters**, a normal download already keeps the full archive layout — do **not** use `-k` or `-pk`.

---

### 3. Cookies (unchanged from original)

For archive.org items that require a logged-in session:

```shell
ia-get --cookies cookies.txt https://archive.org/details/<identifier>
ia-get -b 'logged-in-user=...; logged-in-sig=...' https://archive.org/details/<identifier>
```

With filters:

```shell
ia-get -b cookies.txt https://archive.org/details/<identifier> *ipa
```

**Behaviour:**

- If `--cookies` points to an existing file, it is parsed as Netscape `cookies.txt` (tab-separated, `#HttpOnly_` prefix supported). Otherwise the value is sent as a raw Cookie header.
- Parsed cookies are scoped per request URL (domain, path, expiry, `Secure`).
- Cookies apply to the details-page check, XML metadata fetch, and every file download.

---

### 4. `ia-get.ini` configuration

| Setting | Default | Description |
|---------|---------|-------------|
| `maxbandwidth` | `-1` (unlimited) | Global download cap in **KB/s** (1024-byte KB, not kilobits) |
| `multithreading` | `false` | Download multiple files in parallel |
| `threads` | `4` | Concurrent downloads when `multithreading = true` |

**Example** (`ia-get.ini` in the repo is a commented template):

```ini
maxbandwidth = 4000
multithreading = true
threads = 4
```

**Search order:**

1. `./ia-get.ini` (current working directory)
2. `%APPDATA%\ia-get\ia-get.ini` (Windows)
3. `~/.config/ia-get/ia-get.ini` (Linux/macOS)

The bandwidth limit is **shared across all parallel downloads** (one global cap, not per thread).

---

## Complete usage examples

**Original behaviour — download entire archive with full folder tree:**

```shell
ia-get https://archive.org/details/your-item
```

**List all files and sizes, then exit:**

```shell
ia-get -l https://archive.org/details/your-item
```

**Mobile app dumps — IPA only, flat into current directory:**

```shell
ia-get https://archive.org/details/your-item *ipa
```

**ROM sets — keep folder paths, minimal directories:**

```shell
ia-get https://archive.org/details/your-item *zip *7z -pk
```

**Large archive — skip images and torrent files, download the rest:**

```shell
ia-get https://archive.org/details/your-item #jpg #jpeg #png #torrent
```

**Authenticated item with bandwidth limit (via config file) and parallel downloads:**

```shell
# ia-get.ini: maxbandwidth = 2000, multithreading = true, threads = 3
ia-get -b cookies.txt https://archive.org/details/private-item *pdf
```

**Common mistakes:**

```shell
# Wrong: filter before URL
ia-get *apk https://archive.org/details/your-item

# Wrong: -k without filters (errors)
ia-get https://archive.org/details/your-item -k

# Wrong: both folder modes (errors)
ia-get https://archive.org/details/your-item *apk -k -pk
```

---

## Build from source

You need a [Rust toolchain](https://www.rust-lang.org/tools/install) (1.70+ recommended).  
After building, the binary is at:

| Platform | Release binary |
|----------|----------------|
| Windows | `target\release\ia-get.exe` |
| Linux / macOS | `target/release/ia-get` |

Run tests and lint (optional):

```shell
cargo test
cargo clippy
cargo fmt --check
```

---

### Windows — PowerShell (recommended, system install)

Install Rust with the default MSVC toolchain (requires [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) with the **Desktop development with C++** workload):

```powershell
Invoke-WebRequest -Uri https://win.rustup.rs/x86_64 -OutFile rustup-init.exe
.\rustup-init.exe -y
Remove-Item rustup-init.exe
```

Open a **new** terminal, then clone, build, and run:

```powershell
cd path\to\ia-get
cargo build --release
.\target\release\ia-get.exe --help
```

---

### Windows — PowerShell (GNU toolchain, no Visual Studio)

Use this if you do not want to install Visual Studio. Rust’s bundled MinGW linker is used:

```powershell
Invoke-WebRequest -Uri "https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-gnu/rustup-init.exe" -OutFile rustup-init.exe
.\rustup-init.exe -y --default-host x86_64-pc-windows-gnu --default-toolchain stable-x86_64-pc-windows-gnu
Remove-Item rustup-init.exe
rustup component add rust-mingw
```

```powershell
cd path\to\ia-get
cargo build --release
.\target\release\ia-get.exe --help
```

---

### Windows — CMD

**MSVC toolchain:**

```cmd
curl -o rustup-init.exe https://win.rustup.rs/x86_64
rustup-init.exe -y
del rustup-init.exe
```

Close and reopen CMD, then:

```cmd
cd path\to\ia-get
cargo build --release
target\release\ia-get.exe --help
```

**GNU toolchain (no Visual Studio):**

```cmd
curl -o rustup-init.exe https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-gnu/rustup-init.exe
rustup-init.exe -y --default-host x86_64-pc-windows-gnu --default-toolchain stable-x86_64-pc-windows-gnu
del rustup-init.exe
rustup component add rust-mingw
cd path\to\ia-get
cargo build --release
target\release\ia-get.exe --help
```

---

### Linux / macOS — terminal

Install Rust:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

Build:

```bash
cd /path/to/ia-get
cargo build --release
./target/release/ia-get --help
```

---

### Portable toolchain (optional, project-local)

Install Rust into the project folder instead of your user profile.  
Add `.cargo/` and `.rustup/` to `.gitignore` locally if you use this (they are build artifacts, not source).

**Windows PowerShell:**

```powershell
cd path\to\ia-get
$env:CARGO_HOME = "$PWD\.cargo"
$env:RUSTUP_HOME = "$PWD\.rustup"
Invoke-WebRequest -Uri https://win.rustup.rs/x86_64 -OutFile rustup-init.exe
.\rustup-init.exe -y --no-modify-path
Remove-Item rustup-init.exe
.\.cargo\bin\cargo build --release
.\target\release\ia-get.exe --help
```

**Linux / macOS:**

```bash
cd /path/to/ia-get
export CARGO_HOME="$PWD/.cargo"
export RUSTUP_HOME="$PWD/.rustup"
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
./.cargo/bin/cargo build --release
./target/release/ia-get --help
```

Each new shell session must set `CARGO_HOME` and `RUSTUP_HOME` again (or add those exports to a small wrapper script).

---

### Nix (matches upstream development workflow)

If you use Nix:

```shell
nix develop
cargo build --release
cargo test
```

Or build the flake output directly:

```shell
nix build
```

---

## Original program (unchanged behaviour)

```shell
ia-get https://archive.org/details/your-item
```

- Downloads **all** files from the archive
- Keeps the **full folder layout** (including directories inferred from metadata)
- Resumes partial downloads and verifies MD5 hashes
- Cookies: `--cookies` / `-b`
- Preview files: `--list` / `-l`

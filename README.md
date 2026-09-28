# Zuno

A ridiculously fast API client. Native, keyboard-driven, local-first — built in Rust on
[GPUI](https://crates.io/crates/gpui).

Postman-level capability, Zed-level feel: a virtualized response viewer that holds 60fps on
multi-megabyte JSON, request tabs as editor buffers, curl import, response diffing, and
collections stored as one file per request so they live in git like anything else you own.

[![Rust](https://github.com/aryanjha256/zuno/actions/workflows/rust.yml/badge.svg)](https://github.com/aryanjha256/zuno/actions/workflows/rust.yml)

[![Release](https://github.com/aryanjha256/zuno/actions/workflows/release.yml/badge.svg)](https://github.com/aryanjha256/zuno/actions/workflows/release.yml)

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/aryanjha256/zuno/main/scripts/install.sh | sh
```

The same command installs Zuno and updates it — run it again whenever you want the latest.
It downloads from the latest release and verifies the checksum, then:

- **on Debian, Ubuntu and derivatives** it installs the `.deb` with `apt-get`, asking for
  `sudo` only at that last step;
- **everywhere else** — Fedora, openSUSE, Arch — it unpacks the tarball into `~/.local`, with
  no `sudo` at all;
- **on a Mac with Apple Silicon** it installs `Zuno.app` into `/Applications`, asking for
  `sudo` only if your account cannot write there.

On macOS, use the command rather than downloading the archive in a browser. The app is signed
but not yet notarized, and macOS blocks such an app when a browser downloaded it; installed this
way it opens normally.

`ZUNO_VERSION=0.2.4` pins a specific release if you need to go back, and `ZUNO_METHOD=tarball`
takes the no-`sudo` route on Debian too.

<details>
<summary>Or install the <code>.deb</code> by hand</summary>

Download it from the [latest release](https://github.com/aryanjha256/zuno/releases/latest):

```bash
sudo apt install ./zuno_*_amd64.deb
```

Use `apt install ./file.deb` rather than `dpkg -i` — apt resolves the runtime dependencies,
`dpkg` does not.

</details>

<details>
<summary>Or unpack the tarball by hand</summary>

```bash
tar -xzf zuno-*-x86_64-linux.tar.gz --strip-components=1 -C ~/.local
```

It needs `libxkbcommon`, `libxkbcommon-x11` and `libxcb`, which every desktop already has.

</details>

<details>
<summary>Or run the AppImage — one file, nothing installed</summary>

```bash
chmod +x Zuno-*-x86_64.AppImage && ./Zuno-*-x86_64.AppImage
```

It needs FUSE, which desktop systems have; where it is missing, run it with
`--appimage-extract-and-run`.

</details>

**Requirements:** x86-64 Linux with glibc 2.35+ — Ubuntu 22.04+, Debian 12+, Fedora 36+, or
anything as recent. Zuno renders through Vulkan, so on a machine with no GPU driver installed
you also want one: `mesa-vulkan-drivers` on Debian and Fedora — apt suggests it with the
`.deb`, but only installs it if you accept recommends. On macOS: Apple Silicon, macOS 11 or
later.

Windows is not packaged yet. It builds and passes the test suite in CI.

## Build from source

```bash
sudo apt install libxcb1-dev libxkbcommon-dev libxkbcommon-x11-dev
cargo run --release
```

`--release` matters — a debug build starts roughly 4× slower, which is a bad way to judge
how the app feels.

To build the package itself:

```bash
cargo install cargo-deb --locked
cargo deb -p zuno
```

## Documentation

Three documents, three jobs:

- [`ROADMAP.md`](ROADMAP.md) — what's next and why in that order.
- [`architecture.md`](architecture.md) — how it works, and what was tried and abandoned.
- [`CLAUDE.md`](CLAUDE.md) — commands, invariants, and the traps.

## License

[GPL-3.0-or-later](LICENSE).

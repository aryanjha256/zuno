# Zuno

A ridiculously fast API client. Native, keyboard-driven, local-first — built in Rust on
[GPUI](https://crates.io/crates/gpui).

Postman-level capability, Zed-level feel: a virtualized response viewer that holds 60fps on
multi-megabyte JSON, request tabs as editor buffers, curl import, response diffing, and
collections stored as one file per request so they live in git like anything else you own.

## Install

Download the `.deb` from the [latest release](https://github.com/aryanjha256/zuno/releases/latest):

```bash
sudo apt install ./zuno_*_amd64.deb
```

Use `apt install ./file.deb` rather than `dpkg -i` — apt resolves the runtime dependencies,
`dpkg` does not.

**Requirements:** x86-64, and Ubuntu 22.04+ / Debian 12+ or a derivative (Mint, Pop!\_OS,
elementary). Zuno renders through Vulkan, so on a machine with no GPU driver installed you
also want `mesa-vulkan-drivers` — apt suggests it, but only installs it if you accept
recommends.

No other platform is packaged yet. macOS and Windows both need work beyond building:
keybindings assume `ctrl`, and the config paths assume XDG.

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

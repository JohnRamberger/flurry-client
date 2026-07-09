# flurry-client

PC client for [Flurry](https://github.com/JohnRamberger/flurry) — 3DS screen
streaming over WiFi. Rust rewrite of the old HorizonScreen viewer.

## Status

Early scaffold. The wire protocol is a **draft** — see
[PROTOCOL.md](https://github.com/JohnRamberger/flurry/blob/main/PROTOCOL.md)
in the flurry repo (source of truth); it is expected to be redesigned once the
3DS-side capture/encode loop has been profiled.

## Layout

- `crates/flurry-proto` — pure protocol codec (framing + message types),
  no I/O, unit-tested. Tracks PROTOCOL.md.
- `crates/flurry-client` — GUI application (eframe/egui).

## Build

```
cargo build --release
```

Produces a single native binary at `target/release/flurry-client(.exe)`.

```
cargo test --workspace
```

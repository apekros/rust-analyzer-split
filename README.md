# rust-analyzer-split

Partition a Cargo workspace between a native and a wasm rust-analyzer.

rust-analyzer analyzes one target triple per process
([rust-lang/rust-analyzer#8521](https://github.com/rust-lang/rust-analyzer/issues/8521)),
so a workspace mixing wasm and native crates needs two instances. This proxy
sits between the editor and the `rust-analyzer` on `PATH`, reads the LSP
`initialize` request, and patches `initializationOptions` so that each
instance only sees its half of the workspace:

- `--profile native` excludes the wasm crates.
- `--profile wasm` excludes the native crates and sets `cargo.target` to
  `wasm32-unknown-unknown`.

Both check only the package of the saved file (`check.workspace = false`).

A crate counts as wasm when it has a dependency table gated on a cfg that
matches `wasm32-unknown-unknown`, or depends on `wasm-bindgen` directly.
Override the heuristic in that crate's `Cargo.toml` when it guesses wrong:

```toml
[package.metadata.rust-analyzer-split]
target = "wasm"    # or "native"
```

Without any wasm crate, or outside a Cargo workspace, the native profile runs
rust-analyzer untouched and the wasm profile answers as an empty server, so
the same editor configuration works in every Rust project.

`cargo` and `rust-analyzer` are taken from `PATH`, so the project's pinned
toolchain is used.

## Helix

```toml
[language-server.rust-analyzer]
command = "rust-analyzer-split"
args = ["--profile", "native"]

[language-server.rust-analyzer-wasm]
command = "rust-analyzer-split"
args = ["--profile", "wasm"]

[[language]]
name = "rust"
language-servers = ["rust-analyzer", "rust-analyzer-wasm"]
```

## Tests

`cargo test`. The integration tests write real workspaces to a temp dir, run
real `cargo metadata`, and use a fake `rust-analyzer` script on `PATH`.

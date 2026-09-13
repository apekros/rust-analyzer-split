# rust-analyzer-split

Run two rust-analyzers on one workspace: one for your native crates, one for
your wasm crates.

rust-analyzer can only build for one target at a time
([rust-lang/rust-analyzer#8521](https://github.com/rust-lang/rust-analyzer/issues/8521)).
If your workspace has both wasm and native crates, one of them is always
broken in the editor. This tool fixes that by starting two copies of
rust-analyzer and telling each one which crates to ignore.

It works as a thin wrapper around the `rust-analyzer` on your `PATH`. Your
editor starts it instead of the real thing, and it tweaks the settings the
editor hands over (both the initial ones and any later config reload)
before passing them along:

- `--profile native` hides the wasm crates.
- `--profile wasm` hides the native crates and builds for
  `wasm32-unknown-unknown`.

Both only check the crate you just saved, not the whole workspace.

A crate is treated as wasm if it has dependencies that only apply on
`wasm32-unknown-unknown`, or if it depends on `wasm-bindgen`. If that guess is
wrong, tell it directly in the crate's `Cargo.toml`:

```toml
[package.metadata.rust-analyzer-split]
target = "wasm"    # or "native"
```

If a project has no wasm crates, or isn't a Cargo workspace at all, the
native profile runs plain rust-analyzer and the wasm profile does nothing.
So you can set this up once in your editor and forget about it.

It uses whatever `cargo` and `rust-analyzer` are on your `PATH`, so your
project's toolchain is respected.

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

## Neovim (nvf)

nvf's rust-analyzer preset already wires up `before_init`, `root_dir`, and
friends. Keep all of that, swap the command, and clone the resolved config
into a second server on the Neovim side. (Cloning it in Nix via
`config.vim.lsp.servers.rust-analyzer // { ... }` hits infinite recursion,
nvf deep-compares the whole `servers` set.)

```nix
{ lib, ... }:
{
  vim.languages.rust = {
    enable = true;
    lsp.enable = true;
  };

  vim.lsp.servers.rust-analyzer = {
    # The preset pins cmd to pkgs.rust-analyzer, so it has to be forced.
    cmd = lib.mkForce [ "rust-analyzer-split" "--profile" "native" ];
    settings.rust-analyzer.check.command = "clippy";
  };

  vim.luaConfigRC.rust-analyzer-wasm = lib.nvim.dag.entryAfter [ "lsp-servers" ] ''
    vim.lsp.config["rust-analyzer-wasm"] = vim.tbl_deep_extend("force", vim.lsp.config["rust-analyzer"], {
      cmd = { "rust-analyzer-split", "--profile", "wasm" },
    })
    vim.lsp.enable("rust-analyzer-wasm")
  '';
}
```

Everything under `settings.rust-analyzer` is shared by both servers, and
each gets its own patched copy.

The preset's `:LspCargoReload` command only reloads the client named
`rust-analyzer`, so after changing a `Cargo.toml` restart the wasm one with
`:LspRestart rust-analyzer-wasm`.

## Tests

Run `cargo test`. The integration tests create real workspaces in a temp
folder, run real `cargo metadata`, and stand in a fake `rust-analyzer` script
on `PATH`.

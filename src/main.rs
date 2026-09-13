//! Partition a Cargo workspace between a native and a wasm rust-analyzer.
//!
//! rust-analyzer analyzes one target triple per process (rust-lang/rust-analyzer
//! #8521), so a workspace mixing wasm and native crates needs two instances.
//! This proxy sits between the editor and the `rust-analyzer` on `PATH`, reads
//! the LSP `initialize` request, and patches `initializationOptions` so that
//! each instance only sees its half of the workspace:
//!
//! - `--profile native` excludes the wasm crates.
//! - `--profile wasm` excludes the native crates and sets `cargo.target` to
//!   `wasm32-unknown-unknown`.
//!
//! Both check only the package of the saved file (`check.workspace = false`).
//! rust-analyzer already does that for binary and test targets whatever the
//! setting, so a workspace-wide `--exclude` cannot be relied on, and a saved
//! file that is excluded from the instance has no package, so it triggers no
//! check.
//!
//! A crate counts as wasm when it has a dependency table gated on a cfg that
//! matches `wasm32-unknown-unknown`, or depends on `wasm-bindgen` directly.
//! Override the heuristic in that crate's `Cargo.toml` when it guesses wrong:
//!
//! ```toml
//! [package.metadata.rust-analyzer-split]
//! target = "wasm"    # or "native"
//! ```
//!
//! Without any wasm crate, or outside a Cargo workspace, the native profile
//! runs rust-analyzer untouched and the wasm profile answers as an empty
//! server, so the same editor configuration works in every Rust project.
//!
//! Only the first message is inspected, everything after it is forwarded as-is
//! and rust-analyzer inherits stdout and stderr directly.

mod options;
mod workspace;

use std::io::{self, BufReader, ErrorKind, Read, Stdin, Write};
use std::process::{self, Command, ExitCode, Stdio};
use std::thread;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use lsp_server::{Message, Request, Response};
use serde_json::{Value, json};

use crate::workspace::Target;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Profile {
    Native,
    Wasm,
}

impl From<Profile> for Target {
    fn from(profile: Profile) -> Self {
        match profile {
            Profile::Native => Target::Native,
            Profile::Wasm => Target::Wasm,
        }
    }
}

#[derive(Parser)]
#[command(about = "Partition a Cargo workspace between a native and a wasm rust-analyzer")]
struct Cli {
    /// Which half of the workspace this rust-analyzer instance gets.
    #[arg(long, value_enum)]
    profile: Profile,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("rust-analyzer-split: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let profile = Target::from(Cli::parse().profile);
    let mut stdin = BufReader::new(io::stdin());

    let Some(first) = Message::read(&mut stdin).context("reading initialize")? else {
        return Ok(ExitCode::SUCCESS);
    };
    let mut initialize = match first {
        Message::Request(request) if request.method == "initialize" => request,
        Message::Request(request) => bail!("expected initialize, got {}", request.method),
        Message::Notification(notification) => {
            bail!("expected initialize, got {}", notification.method)
        }
        Message::Response(_) => bail!("expected initialize, got a response"),
    };

    let root = options::root_of(&initialize.params)?;
    let members = workspace::members(&root);

    if profile == Target::Wasm && members.iter().all(|m| m.target != Target::Wasm) {
        return serve_empty(stdin, &initialize);
    }

    options::patch(&mut initialize.params, profile, &members)?;
    run_rust_analyzer(stdin, initialize)
}

/// Answer as a server without capabilities until the client says `exit`.
fn serve_empty(mut stdin: BufReader<Stdin>, initialize: &Request) -> Result<ExitCode> {
    let mut stdout = io::stdout().lock();
    let mut reply = |id, result: Value| -> Result<()> {
        Message::Response(Response::new_ok(id, result)).write(&mut stdout)?;
        stdout.flush()?;
        Ok(())
    };
    reply(initialize.id.clone(), json!({ "capabilities": {} }))?;
    while let Some(message) = Message::read(&mut stdin)? {
        match message {
            Message::Notification(n) if n.method == "exit" => break,
            Message::Request(request) => reply(request.id, Value::Null)?,
            Message::Notification(_) | Message::Response(_) => {}
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Forward the patched `initialize`, then pump stdin until either side ends.
fn run_rust_analyzer(stdin: BufReader<Stdin>, initialize: Request) -> Result<ExitCode> {
    let mut child = match Command::new("rust-analyzer").stdin(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            eprintln!("rust-analyzer-split: rust-analyzer not on PATH");
            return Ok(ExitCode::from(127));
        }
        Err(err) => return Err(err).context("spawning rust-analyzer"),
    };
    let Some(child_stdin) = child.stdin.take() else {
        bail!("rust-analyzer was spawned without a stdin pipe");
    };

    // The pump lives on its own thread so the main thread can wait on the
    // child. A dead rust-analyzer would otherwise leave the editor talking to
    // a proxy blocked forever on stdin.
    thread::spawn(move || pump(stdin, initialize, child_stdin));

    let status = child.wait().context("waiting for rust-analyzer")?;
    let code = status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1);
    // Exit directly rather than returning: the pump thread may be blocked in
    // a read and must not keep the process alive.
    process::exit(code.into())
}

fn pump(mut stdin: impl Read, initialize: Request, mut child_stdin: impl Write) {
    let result = Message::Request(initialize)
        .write(&mut child_stdin)
        .and_then(|()| child_stdin.flush())
        .and_then(|()| io::copy(&mut stdin, &mut child_stdin).map(drop));
    match result {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::BrokenPipe => {}
        Err(err) => eprintln!("rust-analyzer-split: forwarding stdin: {err}"),
    }
    // Dropping child_stdin closes the pipe so rust-analyzer sees EOF too.
}

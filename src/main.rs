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
//! The editor hands the same config over twice: as `initializationOptions`,
//! and again whenever rust-analyzer answers a `workspace/didChangeConfiguration`
//! (which Neovim and Helix both send right after `initialized`) by asking the
//! client for `workspace/configuration`. rust-analyzer replaces its whole
//! client config with that answer, so both copies are patched. Everything else
//! is forwarded untouched and rust-analyzer inherits stderr directly.

mod options;
mod workspace;

use std::collections::HashSet;
use std::io::{self, BufRead, BufReader, ErrorKind, Stdin, Write};
use std::process::{self, Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use lsp_server::{Message, Request, RequestId, Response};
use serde_json::{Value, json};

use crate::workspace::{Member, Target};

const CONFIGURATION_METHOD: &str = "workspace/configuration";

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

    options::patch_initialize(&mut initialize.params, profile, &members)?;
    run_rust_analyzer(stdin, initialize, Split { profile, members })
}

/// What this instance hides, shared by both pump directions.
struct Split {
    profile: Target,
    members: Vec<Member>,
}

/// Ids of `workspace/configuration` requests rust-analyzer has sent and the
/// client has not answered yet. The server pump adds, the client pump takes.
type PendingConfiguration = Arc<Mutex<HashSet<RequestId>>>;

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

/// Forward the patched `initialize`, then pump both directions until
/// rust-analyzer exits.
fn run_rust_analyzer(
    stdin: BufReader<Stdin>,
    initialize: Request,
    split: Split,
) -> Result<ExitCode> {
    let mut child = match Command::new("rust-analyzer")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
    {
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
    let Some(child_stdout) = child.stdout.take() else {
        bail!("rust-analyzer was spawned without a stdout pipe");
    };

    let pending = PendingConfiguration::default();

    // The client pump lives on its own thread so the main thread can drain
    // rust-analyzer's stdout and then wait on it. A dead rust-analyzer would
    // otherwise leave the editor talking to a proxy blocked forever on stdin.
    let client_pending = Arc::clone(&pending);
    thread::spawn(move || pump_client(stdin, initialize, child_stdin, &split, &client_pending));

    pump_server(BufReader::new(child_stdout), io::stdout().lock(), &pending);

    let status = child.wait().context("waiting for rust-analyzer")?;
    let code = status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1);
    // Exit directly rather than returning: the client pump may be blocked in
    // a read and must not keep the process alive.
    process::exit(code.into())
}

/// Editor to rust-analyzer. Answers to `workspace/configuration` get the same
/// patch as `initializationOptions`, everything else passes through.
fn pump_client(
    mut stdin: impl BufRead,
    initialize: Request,
    mut child_stdin: impl Write,
    split: &Split,
    pending: &PendingConfiguration,
) {
    let mut forward = |message: Message| -> io::Result<()> {
        message.write(&mut child_stdin)?;
        child_stdin.flush()
    };
    let run = || -> Result<()> {
        forward(Message::Request(initialize))?;
        while let Some(mut message) = Message::read(&mut stdin)? {
            if let Message::Response(response) = &mut message
                && take_pending(pending, &response.id)
                && let Ok(result) = &mut response.response_result
            {
                options::patch_configuration(result, split.profile, &split.members)?;
            }
            forward(message)?;
        }
        Ok(())
    };
    match run() {
        Ok(()) => {}
        Err(err) if is_broken_pipe(&err) => {}
        Err(err) => eprintln!("rust-analyzer-split: forwarding to rust-analyzer: {err:#}"),
    }
    // Dropping child_stdin closes the pipe so rust-analyzer sees EOF too.
}

/// rust-analyzer to editor. Only records which request ids are
/// `workspace/configuration` so the client pump knows what to patch.
fn pump_server(
    mut child_stdout: impl BufRead,
    mut stdout: impl Write,
    pending: &PendingConfiguration,
) {
    let mut run = || -> Result<()> {
        while let Some(message) = Message::read(&mut child_stdout)? {
            if let Message::Request(request) = &message
                && request.method == CONFIGURATION_METHOD
            {
                lock_pending(pending).insert(request.id.clone());
            }
            message.write(&mut stdout)?;
            stdout.flush()?;
        }
        Ok(())
    };
    match run() {
        Ok(()) => {}
        Err(err) if is_broken_pipe(&err) => {}
        Err(err) => eprintln!("rust-analyzer-split: forwarding to editor: {err:#}"),
    }
}

fn take_pending(pending: &PendingConfiguration, id: &RequestId) -> bool {
    lock_pending(pending).remove(id)
}

fn lock_pending(pending: &PendingConfiguration) -> std::sync::MutexGuard<'_, HashSet<RequestId>> {
    // The set of ids cannot be left half-updated by a panic, so a poisoned
    // lock is still a usable lock.
    pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn is_broken_pipe(err: &anyhow::Error) -> bool {
    err.downcast_ref::<io::Error>()
        .is_some_and(|err| err.kind() == ErrorKind::BrokenPipe)
}

//! End to end tests: a real Cargo workspace in a temp dir, real `cargo
//! metadata`, and a fake `rust-analyzer` shell script first on `PATH`.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{Cursor, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use assert_cmd::cargo::cargo_bin;
use lsp_server::{Message, Notification, Request, RequestId, Response};
use serde_json::{Value, json};
use tempfile::TempDir;

const WASM_TRIPLE: &str = "wasm32-unknown-unknown";

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        Ok(Self {
            dir: tempfile::tempdir()?,
        })
    }

    fn root(&self) -> PathBuf {
        // cargo metadata reports canonical paths, match that so the excludes
        // compare equal (macOS /tmp -> /private/tmp and friends).
        self.dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| self.dir.path().to_path_buf())
    }

    fn bin(&self) -> PathBuf {
        self.dir.path().join("bin")
    }

    fn dump(&self) -> PathBuf {
        self.dir.path().join("rust-analyzer.stdin")
    }

    /// Two-member workspace: `web` gated on a wasm cfg, `app` plain, plus a
    /// `native-ish` crate whose only wasm mention is inside `not(...)`.
    fn write_workspace(&self) -> Result<()> {
        let root = self.dir.path();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"web\", \"native-ish\"]\n",
        )?;
        self.write_crate(
            "app",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
        )?;
        self.write_crate(
            "web",
            "[package]\nname = \"web\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [target.'cfg(target_family = \"wasm\")'.dependencies]\nweb-sys = \"0.3\"\n",
        )?;
        self.write_crate(
            "native-ish",
            "[package]\nname = \"native-ish\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [target.'cfg(not(target_family = \"wasm\"))'.dependencies]\nlibc = \"0.2\"\n",
        )?;
        Ok(())
    }

    /// Single-member workspace with no wasm crate at all.
    fn write_native_only_workspace(&self) -> Result<()> {
        let root = self.dir.path();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n",
        )?;
        self.write_crate(
            "app",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
    }

    fn write_crate(&self, name: &str, manifest: &str) -> Result<()> {
        let dir = self.dir.path().join(name);
        fs::create_dir_all(dir.join("src"))?;
        fs::write(dir.join("Cargo.toml"), manifest)?;
        fs::write(dir.join("src/lib.rs"), "")?;
        Ok(())
    }

    /// Install a fake `rust-analyzer` that runs `body` with `$DUMP` pointing
    /// at [`Self::dump`].
    fn install_fake_rust_analyzer(&self, body: &str) -> Result<()> {
        fs::create_dir_all(self.bin())?;
        let script = self.bin().join("rust-analyzer");
        fs::write(
            &script,
            format!("#!/bin/sh\nDUMP={:?}\n{body}\n", self.dump()),
        )?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
        Ok(())
    }

    fn path_env(&self) -> Result<OsString> {
        let mut paths = vec![self.bin()];
        paths.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
        Ok(env::join_paths(paths)?)
    }

    /// The proxy binary with the fake `rust-analyzer` first on `PATH`.
    fn std_command(&self, profile: &str) -> Result<Command> {
        let mut cmd = Command::new(cargo_bin("rust-analyzer-split"));
        cmd.args(["--profile", profile])
            .env("PATH", self.path_env()?)
            .current_dir(self.dir.path());
        Ok(cmd)
    }

    fn command(&self, profile: &str) -> Result<assert_cmd::Command> {
        Ok(assert_cmd::Command::from_std(self.std_command(profile)?))
    }

    fn initialize(&self, options: Value) -> Request {
        let uri = url::Url::from_directory_path(self.root()).map(|u| u.to_string());
        Request {
            id: RequestId::from(1),
            method: "initialize".into(),
            params: json!({
                "processId": null,
                "rootUri": uri.ok(),
                "capabilities": {},
                "initializationOptions": options,
            }),
        }
    }
}

fn encode(messages: &[Message]) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    for message in messages {
        message.write(&mut buf)?;
    }
    Ok(buf)
}

/// Split the fake rust-analyzer's stdin dump into the parsed first message
/// and every message after it.
fn read_dump(path: &Path) -> Result<(Request, Vec<Message>)> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut cursor = Cursor::new(bytes);
    let Some(Message::Request(first)) = Message::read(&mut cursor)? else {
        bail!("rust-analyzer did not receive a request first");
    };
    let mut rest = Vec::new();
    while let Some(message) = Message::read(&mut cursor)? {
        rest.push(message);
    }
    Ok((first, rest))
}

fn to_json(messages: &[Message]) -> Result<Vec<Value>> {
    messages
        .iter()
        .map(|m| serde_json::to_value(m).map_err(Into::into))
        .collect()
}

fn exclude_paths(request: &Request) -> Vec<String> {
    request.params["initializationOptions"]["files"]["exclude"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn native_profile_excludes_wasm_members_and_forwards_the_rest() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_workspace()?;
    fx.install_fake_rust_analyzer("cat > \"$DUMP\"")?;

    let initialize = fx.initialize(json!({
        "files": { "exclude": ["/keep/me"] },
        "check": { "command": "clippy" },
    }));
    let after = [
        Message::Notification(Notification {
            method: "initialized".into(),
            params: json!({}),
        }),
        Message::Request(Request {
            id: RequestId::from(2),
            method: "textDocument/hover".into(),
            params: json!({ "position": { "line": 1, "character": 2 } }),
        }),
        // A response to some request that is not workspace/configuration
        // must not be touched, even though it looks like a config answer.
        Message::Response(Response::new_ok(
            RequestId::from(7),
            json!([{ "check": { "command": "clippy" } }]),
        )),
    ];
    let mut input = encode(&[Message::Request(initialize)])?;
    input.extend(encode(&after)?);

    fx.command("native")?.write_stdin(input).assert().success();

    let (received, rest) = read_dump(&fx.dump())?;
    assert_eq!(received.method, "initialize");
    assert_eq!(to_json(&rest)?, to_json(&after)?);
    let opts = &received.params["initializationOptions"];
    assert_eq!(
        exclude_paths(&received),
        vec![
            "/keep/me".to_string(),
            fx.root().join("web").to_string_lossy().into_owned()
        ]
    );
    assert_eq!(
        opts["check"],
        json!({ "command": "clippy", "workspace": false })
    );
    assert_eq!(
        opts.get("cargo"),
        None,
        "native profile must not set cargo.target"
    );
    Ok(())
}

#[test]
fn wasm_profile_excludes_native_members_and_sets_target() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_workspace()?;
    fx.install_fake_rust_analyzer("cat > \"$DUMP\"")?;

    let initialize = fx.initialize(json!({}));
    fx.command("wasm")?
        .write_stdin(encode(&[Message::Request(initialize)])?)
        .assert()
        .success();

    let (received, rest) = read_dump(&fx.dump())?;
    assert!(rest.is_empty());
    let mut excludes = exclude_paths(&received);
    excludes.sort();
    let mut expected: Vec<String> = ["app", "native-ish"]
        .iter()
        .map(|m| fx.root().join(m).to_string_lossy().into_owned())
        .collect();
    expected.sort();
    assert_eq!(excludes, expected);
    let opts = &received.params["initializationOptions"];
    assert_eq!(opts["check"]["workspace"], json!(false));
    assert_eq!(opts["cargo"]["target"], json!(WASM_TRIPLE));
    Ok(())
}

#[test]
fn wasm_profile_keeps_client_cargo_target() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_workspace()?;
    fx.install_fake_rust_analyzer("cat > \"$DUMP\"")?;

    let initialize = fx.initialize(json!({ "cargo": { "target": "wasm32-wasip1" } }));
    fx.command("wasm")?
        .write_stdin(encode(&[Message::Request(initialize)])?)
        .assert()
        .success();

    let (received, _) = read_dump(&fx.dump())?;
    assert_eq!(
        received.params["initializationOptions"]["cargo"]["target"],
        json!("wasm32-wasip1")
    );
    Ok(())
}

/// Neovim and Helix send `workspace/didChangeConfiguration` right after
/// `initialized`, rust-analyzer answers by asking `workspace/configuration`,
/// and replaces its whole client config with the answer. That answer needs
/// the same patch as `initializationOptions` or the split is silently undone.
#[test]
fn configuration_answer_is_patched_and_server_output_forwarded() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_workspace()?;
    let configuration = Message::Request(Request {
        id: RequestId::from("cfg-1".to_string()),
        method: "workspace/configuration".into(),
        params: json!({ "items": [{ "section": "rust-analyzer" }] }),
    });
    let progress = Message::Notification(Notification {
        method: "$/progress".into(),
        params: json!({ "token": "rustAnalyzer/Indexing" }),
    });
    // Speak first, then swallow stdin until the editor hangs up.
    let out = encode(&[configuration.clone(), progress.clone()])?;
    fx.install_fake_rust_analyzer(&format!(
        "printf '%s' {} ; cat > \"$DUMP\"",
        shell_quote(std::str::from_utf8(&out)?)
    ))?;

    let mut child = fx
        .std_command("wasm")?
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    let mut stdout = std::io::BufReader::new(child.stdout.take().context("no stdout")?);

    stdin.write_all(&encode(&[Message::Request(fx.initialize(json!({})))])?)?;
    stdin.flush()?;

    // Whatever rust-analyzer says must reach the editor unchanged, in order.
    let mut seen = Vec::new();
    while seen.len() < 2 {
        let Some(message) = Message::read(&mut stdout)? else {
            bail!("proxy closed stdout after {seen:?}");
        };
        seen.push(message);
    }
    assert_eq!(to_json(&seen)?, to_json(&[configuration, progress])?);

    let settings = json!({ "check": { "command": "clippy" } });
    let answers = [
        Message::Response(Response::new_ok(
            RequestId::from("cfg-1".to_string()),
            json!([settings]),
        )),
        // Same id as the answered request, but that one is done: rust-analyzer
        // would have to ask again for this to be a config answer.
        Message::Response(Response::new_ok(
            RequestId::from("cfg-1".to_string()),
            json!([settings]),
        )),
    ];
    stdin.write_all(&encode(&answers)?)?;
    drop(stdin);
    assert!(child.wait()?.success());

    let (_, rest) = read_dump(&fx.dump())?;
    let patched = json!({
        "check": { "command": "clippy", "workspace": false },
        "files": { "exclude": [
            fx.root().join("app").to_string_lossy(),
            fx.root().join("native-ish").to_string_lossy(),
        ] },
        "cargo": { "target": WASM_TRIPLE },
    });
    let mut got = to_json(&rest)?;
    // exclude order follows cargo metadata, which is not stable to assert on.
    if let Some(list) = got[0]["result"][0]["files"]["exclude"].as_array_mut() {
        list.sort_by_key(|v| v.as_str().map(str::to_owned));
    }
    assert_eq!(
        got,
        vec![
            json!({ "id": "cfg-1", "result": [patched] }),
            json!({ "id": "cfg-1", "result": [settings] }),
        ]
    );
    Ok(())
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[test]
fn wasm_profile_without_wasm_crates_serves_empty() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_native_only_workspace()?;
    fx.install_fake_rust_analyzer("echo 'should not run' >&2; exit 42")?;

    let initialize = fx.initialize(json!({}));
    let hover = Request {
        id: RequestId::from(2),
        method: "textDocument/hover".into(),
        params: json!({}),
    };
    let initialized = Notification {
        method: "initialized".into(),
        params: json!({}),
    };
    let shutdown = Request {
        id: RequestId::from("s".to_string()),
        method: "shutdown".into(),
        params: Value::Null,
    };
    let exit = Notification {
        method: "exit".into(),
        params: Value::Null,
    };
    let input = encode(&[
        Message::Request(initialize),
        Message::Notification(initialized),
        Message::Request(hover),
        Message::Request(shutdown),
        Message::Notification(exit),
        // Anything after exit must be ignored, the proxy must not read it
        // as an LSP message.
        Message::Request(Request {
            id: RequestId::from(99),
            method: "after/exit".into(),
            params: Value::Null,
        }),
    ])?;

    let output = fx
        .command("wasm")?
        .write_stdin(input)
        .assert()
        .success()
        .get_output()
        .clone();
    assert!(!fx.dump().exists(), "rust-analyzer must not be started");
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut cursor = Cursor::new(output.stdout);
    let mut responses = Vec::new();
    while let Some(message) = Message::read(&mut cursor)? {
        let Message::Response(response) = message else {
            bail!("empty server sent a non-response: {message:?}");
        };
        responses.push(serde_json::to_value(response)?);
    }
    assert_eq!(
        responses,
        vec![
            json!({ "id": 1, "result": { "capabilities": {} } }),
            json!({ "id": 2, "result": null }),
            json!({ "id": "s", "result": null }),
        ]
    );
    Ok(())
}

#[test]
fn proxy_exits_with_rust_analyzer_status_while_stdin_is_open() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_workspace()?;
    fx.install_fake_rust_analyzer("exit 3")?;

    let mut child = fx
        .std_command("native")?
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    stdin.write_all(&encode(&[Message::Request(fx.initialize(json!({})))])?)?;
    stdin.flush()?;

    // Keep stdin open, like an editor that has not noticed the server died.
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            child.kill()?;
            bail!("proxy did not exit after rust-analyzer died");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(stdin);
    assert_eq!(status.code(), Some(3));
    Ok(())
}

#[test]
fn non_cargo_directory_runs_rust_analyzer_untouched() -> Result<()> {
    let fx = Fixture::new()?;
    fx.install_fake_rust_analyzer("cat > \"$DUMP\"")?;

    let options = json!({ "check": { "command": "clippy" }, "files": { "exclude": ["/x"] } });
    let initialize = fx.initialize(options.clone());
    let sent = serde_json::to_value(&initialize)?;
    fx.command("native")?
        .write_stdin(encode(&[Message::Request(initialize)])?)
        .assert()
        .success();

    let (received, _) = read_dump(&fx.dump())?;
    let mut expected = sent;
    expected["params"]["initializationOptions"]["check"]["workspace"] = json!(false);
    assert_eq!(serde_json::to_value(&received)?, expected);
    Ok(())
}

#[test]
fn missing_rust_analyzer_exits_127() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_workspace()?;
    // PATH is only the fixture bin dir holding links to the real cargo and
    // rustc, so cargo metadata still works but rust-analyzer is nowhere.
    fs::create_dir_all(fx.bin())?;
    for tool in ["cargo", "rustc"] {
        std::os::unix::fs::symlink(which(tool)?, fx.bin().join(tool))?;
    }
    let path = env::join_paths([fx.bin()])?;

    assert_cmd::Command::new(cargo_bin("rust-analyzer-split"))
        .args(["--profile", "native"])
        .env("PATH", path)
        .current_dir(fx.dir.path())
        .write_stdin(encode(&[Message::Request(fx.initialize(json!({})))])?)
        .assert()
        .code(127)
        .stderr("rust-analyzer-split: rust-analyzer not on PATH\n");
    Ok(())
}

#[test]
fn empty_stdin_is_a_clean_exit() -> Result<()> {
    let fx = Fixture::new()?;
    fx.command("native")?
        .write_stdin("")
        .assert()
        .success()
        .stdout("");
    Ok(())
}

#[test]
fn first_message_must_be_initialize() -> Result<()> {
    let fx = Fixture::new()?;
    let hover = Request {
        id: RequestId::from(1),
        method: "textDocument/hover".into(),
        params: json!({}),
    };
    fx.command("native")?
        .write_stdin(encode(&[Message::Request(hover)])?)
        .assert()
        .failure()
        .stderr("rust-analyzer-split: expected initialize, got textDocument/hover\n");
    Ok(())
}

fn which(program: &str) -> Result<PathBuf> {
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(program))
        .find(|path| path.is_file())
        .with_context(|| format!("{program} not on PATH"))
}

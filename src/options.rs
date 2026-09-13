//! Patching of the `initialize` request: workspace root resolution and the
//! `initializationOptions` edits that hide the other profile's crates.

use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use url::Url;

use crate::workspace::{Member, Target, WASM_TRIPLE};

/// Workspace root from `params.rootUri`, then `params.rootPath`, then cwd.
pub fn root_of(params: &Value) -> Result<PathBuf> {
    if let Some(uri) = params.get("rootUri").and_then(Value::as_str) {
        let url = Url::parse(uri).with_context(|| format!("invalid rootUri {uri:?}"))?;
        if url.scheme() == "file" {
            return url
                .to_file_path()
                .map_err(|()| anyhow::anyhow!("rootUri {uri:?} is not a local path"));
        }
    }
    if let Some(path) = params.get("rootPath").and_then(Value::as_str) {
        return Ok(PathBuf::from(path));
    }
    env::current_dir().context("cannot determine cwd")
}

/// Edit `params.initializationOptions` in place so that this `profile`'s
/// rust-analyzer only sees its half of the workspace.
pub fn patch(params: &mut Value, profile: Target, members: &[Member]) -> Result<()> {
    if params.is_null() {
        *params = json!({});
    }
    let Some(params) = params.as_object_mut() else {
        bail!("initialize params is not an object");
    };
    let options = params
        .entry("initializationOptions")
        .or_insert_with(|| json!({}));
    if !options.is_object() {
        *options = json!({});
    }
    let Some(options) = options.as_object_mut() else {
        unreachable!("initializationOptions was just made an object");
    };

    let excluded = members.iter().filter(|m| m.target != profile);
    let exclude = table(options, "files")?
        .entry("exclude")
        .or_insert_with(|| json!([]));
    let Some(exclude) = exclude.as_array_mut() else {
        bail!("initializationOptions.files.exclude is not an array");
    };
    exclude.extend(excluded.map(|m| Value::String(m.directory.to_string_lossy().into_owned())));

    table(options, "check")?.insert("workspace".into(), Value::Bool(false));

    if profile == Target::Wasm {
        table(options, "cargo")?
            .entry("target")
            .or_insert_with(|| Value::String(WASM_TRIPLE.into()));
    }
    Ok(())
}

fn table<'a>(options: &'a mut Map<String, Value>, key: &str) -> Result<&'a mut Map<String, Value>> {
    let value = options.entry(key).or_insert_with(|| json!({}));
    value
        .as_object_mut()
        .with_context(|| format!("initializationOptions.{key} is not a table"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn members() -> Vec<Member> {
        vec![
            Member {
                directory: "/ws/app".into(),
                target: Target::Native,
            },
            Member {
                directory: "/ws/web".into(),
                target: Target::Wasm,
            },
        ]
    }

    #[test]
    fn root_from_percent_encoded_uri() -> Result<()> {
        let params = json!({ "rootUri": "file:///home/me/my%20project", "rootPath": "/nope" });
        assert_eq!(root_of(&params)?, PathBuf::from("/home/me/my project"));
        Ok(())
    }

    #[test]
    fn root_falls_back_to_root_path() -> Result<()> {
        let params = json!({ "rootUri": null, "rootPath": "/ws" });
        assert_eq!(root_of(&params)?, PathBuf::from("/ws"));
        let params = json!({ "rootPath": "/ws" });
        assert_eq!(root_of(&params)?, PathBuf::from("/ws"));
        Ok(())
    }

    #[test]
    fn root_falls_back_to_cwd() -> Result<()> {
        assert_eq!(root_of(&json!({}))?, env::current_dir()?);
        assert_eq!(root_of(&Value::Null)?, env::current_dir()?);
        Ok(())
    }

    #[test]
    fn native_creates_tables_and_excludes_wasm() -> Result<()> {
        let mut params = Value::Null;
        patch(&mut params, Target::Native, &members())?;
        assert_eq!(
            params,
            json!({
                "initializationOptions": {
                    "files": { "exclude": ["/ws/web"] },
                    "check": { "workspace": false },
                }
            })
        );
        Ok(())
    }

    #[test]
    fn wasm_excludes_native_and_sets_target() -> Result<()> {
        let mut params = json!({ "initializationOptions": "garbage" });
        patch(&mut params, Target::Wasm, &members())?;
        assert_eq!(
            params,
            json!({
                "initializationOptions": {
                    "files": { "exclude": ["/ws/app"] },
                    "check": { "workspace": false },
                    "cargo": { "target": WASM_TRIPLE },
                }
            })
        );
        Ok(())
    }

    #[test]
    fn existing_settings_are_preserved() -> Result<()> {
        let mut params = json!({
            "initializationOptions": {
                "files": { "exclude": ["/ws/vendor"], "watcher": "server" },
                "check": { "command": "clippy", "workspace": true },
                "cargo": { "target": "wasm32-wasip1", "targetDir": true },
                "procMacro": { "enable": true },
            }
        });
        patch(&mut params, Target::Wasm, &members())?;
        assert_eq!(
            params["initializationOptions"],
            json!({
                "files": { "exclude": ["/ws/vendor", "/ws/app"], "watcher": "server" },
                "check": { "command": "clippy", "workspace": false },
                "cargo": { "target": "wasm32-wasip1", "targetDir": true },
                "procMacro": { "enable": true },
            })
        );
        Ok(())
    }

    #[test]
    fn non_table_keys_are_errors() {
        for key in ["files", "check", "cargo"] {
            let mut params = json!({ "initializationOptions": { key: 1 } });
            assert!(
                patch(&mut params, Target::Wasm, &members()).is_err(),
                "{key}"
            );
        }
        let mut params = json!({ "initializationOptions": { "files": { "exclude": "x" } } });
        assert!(patch(&mut params, Target::Native, &members()).is_err());
        let mut params = json!([]);
        assert!(patch(&mut params, Target::Native, &members()).is_err());
    }
}

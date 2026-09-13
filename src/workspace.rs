//! Cargo workspace discovery and per-member target classification.

use std::fmt;
use std::path::{Path, PathBuf};

use cargo_metadata::{MetadataCommand, Package};
use cfg_expr::targets::{TargetInfo, get_builtin_target_by_triple};
use cfg_expr::{Expression, Predicate};
use serde::Deserialize;

pub const WASM_TRIPLE: &str = "wasm32-unknown-unknown";

/// Key of the `[package.metadata.<key>]` override table in a member's
/// `Cargo.toml`.
const METADATA_KEY: &str = "rust-analyzer-split";

/// Which rust-analyzer instance a workspace member belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    Native,
    Wasm,
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Target::Native => "native",
            Target::Wasm => "wasm",
        })
    }
}

/// `[package.metadata.rust-analyzer-split]` as written by the user.
#[derive(Debug, Deserialize)]
struct Override {
    target: Target,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// Directory containing the member's `Cargo.toml`, absolute.
    pub directory: PathBuf,
    pub target: Target,
}

/// Members of the Cargo workspace at `root`. Empty when `cargo` is not on
/// `PATH` or `root` is not inside a Cargo project.
pub fn members(root: &Path) -> Vec<Member> {
    let Ok(metadata) = MetadataCommand::new().current_dir(root).no_deps().exec() else {
        return Vec::new();
    };
    metadata
        .workspace_packages()
        .into_iter()
        .map(|package| Member {
            directory: package
                .manifest_path
                .parent()
                .map(|dir| dir.as_std_path().to_path_buf())
                .unwrap_or_else(|| package.manifest_path.as_std_path().to_path_buf()),
            target: classify(package),
        })
        .collect()
}

fn classify(package: &Package) -> Target {
    let override_target = package
        .metadata
        .get(METADATA_KEY)
        .and_then(|table| serde_json::from_value::<Override>(table.clone()).ok())
        .map(|o| o.target);
    let dependencies = package.dependencies.iter().map(|dep| {
        (
            dep.name.as_str(),
            dep.target.as_ref().map(|p| p.to_string()),
        )
    });
    classify_from(override_target, dependencies)
}

/// The heuristic behind [`classify`], separated from `cargo_metadata`'s
/// non-constructible structs so it can be unit tested. Each dependency is
/// `(name, target platform as cargo prints it)`.
fn classify_from<'a>(
    override_target: Option<Target>,
    dependencies: impl IntoIterator<Item = (&'a str, Option<String>)>,
) -> Target {
    if let Some(target) = override_target {
        return target;
    }
    let is_wasm = dependencies.into_iter().any(|(name, platform)| {
        name == "wasm-bindgen" || platform.is_some_and(|p| platform_is_wasm(&p))
    });
    if is_wasm {
        Target::Wasm
    } else {
        Target::Native
    }
}

/// Whether a `[target.<platform>.dependencies]` key applies to the wasm
/// target. `platform` is either a bare triple or a `cfg(...)` expression.
fn platform_is_wasm(platform: &str) -> bool {
    let Some(wasm) = wasm_target() else {
        return false;
    };
    match Expression::parse(platform) {
        Ok(expr) => expr.eval(|pred| match pred {
            Predicate::Target(tp) => tp.matches(wasm),
            _ => false,
        }),
        Err(_) => platform == WASM_TRIPLE,
    }
}

fn wasm_target() -> Option<&'static TargetInfo> {
    get_builtin_target_by_triple(WASM_TRIPLE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_wins_over_heuristic() {
        let deps = [("wasm-bindgen", None)];
        assert_eq!(classify_from(Some(Target::Native), deps), Target::Native);
        assert_eq!(classify_from(Some(Target::Wasm), []), Target::Wasm);
    }

    #[test]
    fn override_table_deserializes() -> anyhow::Result<()> {
        let table = serde_json::json!({ "target": "wasm" });
        let parsed: Override = serde_json::from_value(table)?;
        assert_eq!(parsed.target, Target::Wasm);
        let bogus = serde_json::json!({ "target": "riscv" });
        assert!(serde_json::from_value::<Override>(bogus).is_err());
        Ok(())
    }

    #[test]
    fn wasm_bindgen_dependency_is_wasm() {
        let deps = [("serde", None), ("wasm-bindgen", None)];
        assert_eq!(classify_from(None, deps), Target::Wasm);
    }

    #[test]
    fn plain_dependencies_are_native() {
        let deps = [("serde", None), ("tokio", Some("cfg(unix)".to_string()))];
        assert_eq!(classify_from(None, deps), Target::Native);
    }

    #[test]
    fn cfg_matching_wasm_is_wasm() {
        assert!(platform_is_wasm(r#"cfg(target_family = "wasm")"#));
        assert!(platform_is_wasm(r#"cfg(target_arch = "wasm32")"#));
        assert!(platform_is_wasm(
            r#"cfg(any(target_os = "linux", target_arch = "wasm32"))"#
        ));
        assert!(platform_is_wasm(
            r#"cfg(all(target_arch = "wasm32", not(unix)))"#
        ));
    }

    #[test]
    fn negated_wasm_cfg_is_native() {
        // The Python version substring matched "wasm" and got this wrong.
        assert!(!platform_is_wasm(r#"cfg(not(target_family = "wasm"))"#));
        assert!(!platform_is_wasm(r#"cfg(not(target_arch = "wasm32"))"#));
        let deps = [(
            "libc",
            Some(r#"cfg(not(target_family = "wasm"))"#.to_string()),
        )];
        assert_eq!(classify_from(None, deps), Target::Native);
    }

    #[test]
    fn bare_triple_matches_exactly() {
        assert!(platform_is_wasm("wasm32-unknown-unknown"));
        assert!(!platform_is_wasm("wasm32-wasip1"));
        assert!(!platform_is_wasm("x86_64-unknown-linux-gnu"));
    }
}

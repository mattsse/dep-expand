//! dep-expand
//!
//! Expand cargo dependencies during build.
//!
//! # Example expand `anyhow` dependency
//!
//! ```no_run
//! # use dep_expand::Expander;
//! let expander = Expander::default();
//! let output = expander
//!         .expand("anyhow").unwrap();
//! ```
//!

use anyhow::Context;
use cargo_metadata::{CargoOpt, Metadata, MetadataCommand, Package};
use quote::quote;

use std::{
    env,
    ffi::OsString,
    fmt::{Display, Formatter},
    path::Path,
    process::{Command, Stdio},
};
use syn_select::Selector;

/// Helper Error to determine whether `cargo metadata` failed for registry crates
#[derive(Debug)]
struct MissingWorkspace;

impl Display for MissingWorkspace {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("virtual manifests must be configured with [workspace]")
    }
}

impl std::error::Error for MissingWorkspace {}

/// How to apply cargo expand
// Based on https://github.com/dtolnay/cargo-expand
#[derive(Debug, Clone, Default)]
pub struct Expander {
    /// features to activate
    pub features: Vec<String>,
    /// Activate all available features
    pub all_features: bool,
    /// Do not activate the `default` feature
    pub no_default_features: bool,
    /// Include tests when expanding the lib or bin
    pub tests: bool,
    /// Build artifacts in release mode, with optimizations
    pub release: bool,
    /// Unstable (nightly-only) flags to Cargo
    pub unstable_flags: Vec<String>,
    /// The manifest path of the targeted crate, default is the current
    pub manifest_path: Option<String>,
}

impl Expander {
    pub fn add_feature(mut self, s: impl Into<String>) -> Self {
        self.features.push(s.into());
        self
    }

    pub fn add_unstable_flag(mut self, s: impl Into<String>) -> Self {
        self.unstable_flags.push(s.into());
        self
    }

    pub fn with_manifest(mut self, s: impl Into<String>) -> Self {
        self.manifest_path = Some(s.into());
        self
    }

    pub fn with_tests(mut self) -> Self {
        self.tests = true;
        self
    }

    pub fn with_all_features(mut self) -> Self {
        self.all_features = true;
        self
    }

    pub fn with_no_default_features(mut self) -> Self {
        self.no_default_features = true;
        self
    }

    pub fn with_release(mut self) -> Self {
        self.release = true;
        self
    }

    /// Returns the expanded lib of the given dependency.
    // Based on https://github.com/rsolomo/cargo-check
    pub fn expand(&self, package: impl AsRef<str>) -> anyhow::Result<String> {
        let package = package.as_ref();
        let pkg = self.find_package(package)?;

        let manifest_path = pkg.manifest_path.to_string();

        // try to run cargo if it fails due to virtual manifest
        match self.run_cargo(manifest_path) {
            res @ Ok(_) => res,
            Err(err) => {
                match err.downcast::<MissingWorkspace>() {
                    Ok(_) => {
                        // need to make a copy for registry packages due to virtual manifests
                        let tmp =
                            tempdir::TempDir::new(&format!("dep-expand-{}", pkg.name)).unwrap();
                        let dep_path = pkg
                            .manifest_path
                            .parent()
                            .context("Failed to find dependency dir")?;
                        let package_dir = dep_path.file_name().unwrap();
                        let tmp_package_dir = tmp.path().join(package_dir);

                        // copy the package to temp location
                        fs_extra::dir::copy(
                            dep_path,
                            tmp.path(),
                            &fs_extra::dir::CopyOptions::default(),
                        )?;

                        // try run again but on the copy
                        self.run_cargo(tmp_package_dir.join("Cargo.toml"))
                    }
                    Err(err) => Err(err),
                }
            }
        }
    }

    fn run_cargo(&self, manifest_path: impl AsRef<Path>) -> anyhow::Result<String> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("dep-expand");
        let outdir = builder.tempdir().expect("failed to create tmp file");
        let outfile_path = outdir.path().join("expanded");

        let mut cmd = Command::new(cargo_binary());
        cmd.arg("rustc");

        if self.tests {
            cmd.arg("--profile=test");
        } else {
            cmd.arg("--profile=check");
        }

        if self.release {
            cmd.arg("--release");
        }

        if !self.features.is_empty() {
            cmd.arg("--features").arg(self.features.join(" "));
        }

        if self.all_features {
            cmd.arg("--all-features");
        }

        if self.no_default_features {
            cmd.arg("--no-default-features");
        }

        cmd.arg("--lib")
            .arg("--manifest-path")
            .arg(manifest_path.as_ref());

        for unstable_flag in &self.unstable_flags {
            cmd.arg("-Z");
            cmd.arg(unstable_flag);
        }

        cmd.arg("--")
            .arg("-o")
            .arg(&outfile_path)
            .arg("-Zunstable-options")
            .arg("--pretty=expanded");

        let output = cmd.stderr(Stdio::piped()).spawn()?.wait_with_output()?;

        let err = String::from_utf8_lossy(&output.stderr);
        if err.starts_with("error: failed to parse manifest at")
            && err
                .trim_end()
                .ends_with("virtual manifests must be configured with [workspace]")
        {
            return Err(MissingWorkspace {}.into());
        }

        let content = std::fs::read_to_string(&outfile_path)?;
        if content.is_empty() {
            anyhow::bail!("ERROR: rustc produced no expanded output");
        }
        Ok(content)
    }

    /// Returns the given path of the given dependency.
    pub fn expand_path(&self, package: impl AsRef<str>, path: Selector) -> anyhow::Result<String> {
        let content = self.expand(package)?;
        filter(content, path)
    }

    fn get_metadata(&self) -> anyhow::Result<Metadata> {
        Ok(MetadataCommand::new()
            .manifest_path(self.manifest_path.as_deref().unwrap_or(&format!(
                "{}/Cargo.toml",
                env::var("CARGO_MANIFEST_DIR").expect("No Manifest found")
            )))
            .features(CargoOpt::AllFeatures)
            .exec()?)
    }

    fn find_package(&self, name: impl AsRef<str>) -> anyhow::Result<Package> {
        let name = name.as_ref();
        let metadata = self.get_metadata()?;
        metadata
            .packages
            .into_iter()
            .find(|pkg| pkg.name == name)
            .context(format!("No package found with matching name: `{}`", name))
    }
}

fn cargo_binary() -> OsString {
    env::var_os("CARGO").unwrap_or_else(|| "cargo".to_owned().into())
}

/// Applies the filter on the content
pub fn filter(mut content: String, filter: Selector) -> anyhow::Result<String> {
    let mut syntax_tree = syn::parse_file(&content)?;
    syntax_tree.shebang = None;
    syntax_tree.attrs.clear();
    syntax_tree.items = filter.apply_to(&syntax_tree);
    content = quote!(#syntax_tree).to_string();
    Ok(content)
}

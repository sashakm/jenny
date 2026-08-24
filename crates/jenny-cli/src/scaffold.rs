//! Turning templates into files on disk.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::names::{ProjectNames, ResourceNames};
use crate::template::{Bindings, render};

/// Which optional pieces a generated project wants.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Features(BTreeSet<String>);

impl Features {
    /// The features a generated service may ask for. Anything else is a typo, and
    /// scaffolding silently ignoring one would produce a project missing the very thing
    /// it was generated for.
    pub const KNOWN: &'static [&'static str] = &[
        "persistence",
        "messaging",
        "security",
        "openapi",
        "swagger-ui",
        "telemetry",
    ];

    pub fn parse(raw: &[String]) -> Result<Self> {
        let mut features = BTreeSet::new();
        for entry in raw {
            for name in entry.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                if !Self::KNOWN.contains(&name) {
                    bail!(
                        "unknown feature `{name}`; choose from: {}",
                        Self::KNOWN.join(", ")
                    );
                }
                features.insert(name.to_string());
            }
        }
        Ok(Self(features))
    }

    pub fn has(&self, name: &str) -> bool {
        self.0.contains(name)
    }

    /// The features to put in the generated `Cargo.toml`.
    ///
    /// `server` is always there — a jenny service without the runtime is not a service —
    /// and `telemetry` maps onto the OTLP backend rather than being a feature of its own.
    pub fn cargo_list(&self) -> String {
        let mut names = vec!["\"server\"".to_string()];
        for feature in &self.0 {
            let mapped = match feature.as_str() {
                "telemetry" => "telemetry-otlp",
                other => other,
            };
            names.push(format!("\"{mapped}\""));
        }
        names.join(", ")
    }
}

/// Where a generated file goes and what it should contain.
pub struct Plan {
    root: PathBuf,
    files: Vec<(PathBuf, String)>,
    /// Lines to add to an existing `mod.rs`, keyed by file.
    module_lines: Vec<(PathBuf, String)>,
}

impl Plan {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            files: Vec::new(),
            module_lines: Vec::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn add(&mut self, relative: impl Into<PathBuf>, contents: String) {
        self.files.push((relative.into(), contents));
    }

    /// Record a `pub mod …;` to insert into an existing module file.
    pub fn declare_module(&mut self, relative: impl Into<PathBuf>, module: &str) {
        self.module_lines
            .push((relative.into(), format!("pub mod {module};")));
    }

    pub fn paths(&self) -> Vec<&Path> {
        self.files.iter().map(|(path, _)| path.as_path()).collect()
    }

    /// Fail if any file already exists, before writing anything.
    ///
    /// Checked up front so a name collision cannot leave a half-generated tree behind.
    pub fn ensure_clear(&self) -> Result<()> {
        for (relative, _) in &self.files {
            let path = self.root.join(relative);
            if path.exists() {
                bail!(
                    "{} already exists; refusing to overwrite it",
                    path.display()
                );
            }
        }
        Ok(())
    }

    pub fn write(&self) -> Result<()> {
        self.ensure_clear()?;

        for (relative, contents) in &self.files {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(&path, contents)
                .with_context(|| format!("writing {}", path.display()))?;
        }

        for (relative, line) in &self.module_lines {
            append_module_line(&self.root.join(relative), line)?;
        }
        Ok(())
    }
}

/// Add `pub mod x;` to a module file, keeping the declarations sorted and unique.
fn append_module_line(path: &Path, line: &str) -> Result<()> {
    let existing = std::fs::read_to_string(path)
        .with_context(|| format!("reading {} to add `{line}`", path.display()))?;

    if existing.lines().any(|l| l.trim() == line) {
        return Ok(());
    }

    // Module declarations sit in one contiguous block; keep them together and sorted so
    // repeated generation produces a diff of exactly one line.
    let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();
    let first = lines
        .iter()
        .position(|l| l.trim_start().starts_with("pub mod "));
    match first {
        Some(start) => {
            let end = lines[start..]
                .iter()
                .position(|l| !l.trim_start().starts_with("pub mod "))
                .map(|offset| start + offset)
                .unwrap_or(lines.len());
            lines.insert(end, line.to_string());
            let last = end.min(lines.len() - 1);
            lines[start..=last].sort();
        }
        None => {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push(line.to_string());
        }
    }

    std::fs::write(path, format!("{}\n", lines.join("\n")))
        .with_context(|| format!("writing {}", path.display()))
}

/// Bindings shared by every template.
fn base_bindings(project: &ProjectNames, features: &Features) -> Bindings {
    Bindings::new()
        .set("project_kebab", project.kebab.clone())
        .set("project_snake", project.snake.clone())
        .flag("persistence", features.has("persistence"))
        .flag("messaging", features.has("messaging"))
        .flag("security", features.has("security"))
        .flag(
            "openapi",
            features.has("openapi") || features.has("swagger-ui"),
        )
        .flag("telemetry", features.has("telemetry"))
}

fn with_resource(bindings: Bindings, resource: &ResourceNames) -> Bindings {
    bindings
        .set("resource_snake", resource.snake.clone())
        .set("resource_pascal", resource.pascal.clone())
        .set("resource_plural", resource.plural.clone())
        .set("resource_screaming", resource.screaming.clone())
}

macro_rules! project_template {
    ($name:literal) => {
        include_str!(concat!("../templates/project/", $name))
    };
}

macro_rules! resource_template {
    ($name:literal) => {
        include_str!(concat!("../templates/resource/", $name))
    };
}

/// Plan a whole new project.
pub fn plan_project(
    root: impl Into<PathBuf>,
    project: &ProjectNames,
    resource: &ResourceNames,
    features: &Features,
    jenny_dependency: &str,
) -> Result<Plan> {
    let mut plan = Plan::new(root);
    let bindings = with_resource(base_bindings(project, features), resource)
        .set("jenny_features", features.cargo_list())
        .set("jenny_dependency", jenny_dependency)
        // The compose file needs to know whether it has any services at all.
        .flag(
            "no_services",
            !features.has("persistence")
                && !features.has("messaging")
                && !features.has("telemetry"),
        );

    for (path, source) in [
        ("Cargo.toml", project_template!("Cargo.toml.tmpl")),
        (".gitignore", project_template!("gitignore.tmpl")),
        ("config.yaml", project_template!("config.yaml.tmpl")),
        ("justfile", project_template!("justfile.tmpl")),
        ("Dockerfile", project_template!("Dockerfile.tmpl")),
        ("compose.yaml", project_template!("compose.yaml.tmpl")),
        ("README.md", project_template!("README.md.tmpl")),
        ("src/lib.rs", project_template!("lib.rs.tmpl")),
        ("src/main.rs", project_template!("main.rs.tmpl")),
        ("src/domain/mod.rs", project_template!("domain_mod.rs.tmpl")),
        (
            "src/application/mod.rs",
            project_template!("application_mod.rs.tmpl"),
        ),
        (
            "src/adapter/mod.rs",
            project_template!("adapter_mod.rs.tmpl"),
        ),
        (
            "src/adapter/inbound/mod.rs",
            project_template!("adapter_inbound_mod.rs.tmpl"),
        ),
        (
            "src/adapter/outbound/mod.rs",
            project_template!("adapter_outbound_mod.rs.tmpl"),
        ),
        (
            "src/adapter/outbound/events.rs",
            project_template!("events.rs.tmpl"),
        ),
    ] {
        plan.add(path, render(source, &bindings)?);
    }

    // The module files a resource declares itself into.
    for (path, doc) in [
        ("src/domain/model/mod.rs", "//! Entities and value objects."),
        (
            "src/domain/event/mod.rs",
            "//! Domain event definitions — no dispatch.",
        ),
        (
            "src/domain/port/mod.rs",
            "//! Outbound ports the domain owns.",
        ),
        (
            "src/adapter/inbound/rest/mod.rs",
            "//! HTTP in: DTOs, conversions, status codes.",
        ),
    ] {
        plan.add(path, format!("{doc}\n"));
    }

    add_resource_files(&mut plan, resource, features, /* declare = */ true)?;
    Ok(plan)
}

/// Plan the files for one resource, across every ring.
pub fn plan_resource(
    root: impl Into<PathBuf>,
    project: &ProjectNames,
    resource: &ResourceNames,
    features: &Features,
) -> Result<Plan> {
    let mut plan = Plan::new(root);
    let _ = base_bindings(project, features);
    add_resource_files(&mut plan, resource, features, true)?;
    Ok(plan)
}

fn add_resource_files(
    plan: &mut Plan,
    resource: &ResourceNames,
    features: &Features,
    declare: bool,
) -> Result<()> {
    let project = ProjectNames::parse("placeholder")?;
    let bindings = with_resource(base_bindings(&project, features), resource);

    let snake = &resource.snake;
    let files = [
        (
            format!("src/domain/model/{snake}.rs"),
            resource_template!("model.rs.tmpl"),
        ),
        (
            format!("src/domain/event/{snake}.rs"),
            resource_template!("event.rs.tmpl"),
        ),
        (
            format!("src/domain/port/{snake}_repository.rs"),
            resource_template!("port.rs.tmpl"),
        ),
        (
            format!("src/application/{snake}.rs"),
            resource_template!("application.rs.tmpl"),
        ),
        (
            format!("src/adapter/inbound/rest/{snake}.rs"),
            resource_template!("rest.rs.tmpl"),
        ),
        (
            format!("src/adapter/outbound/{snake}_repository.rs"),
            resource_template!("repository.rs.tmpl"),
        ),
    ];

    for (path, source) in files {
        plan.add(path, render(source, &bindings)?);
    }

    if declare {
        plan.declare_module("src/domain/model/mod.rs", snake);
        plan.declare_module("src/domain/event/mod.rs", snake);
        plan.declare_module("src/domain/port/mod.rs", &format!("{snake}_repository"));
        plan.declare_module("src/application/mod.rs", snake);
        plan.declare_module("src/adapter/inbound/rest/mod.rs", snake);
        plan.declare_module(
            "src/adapter/outbound/mod.rs",
            &format!("{snake}_repository"),
        );
    }
    Ok(())
}

/// The integration test that ships with a new project.
pub fn plan_project_test(
    plan: &mut Plan,
    project: &ProjectNames,
    resource: &ResourceNames,
    features: &Features,
) -> Result<()> {
    let bindings = with_resource(base_bindings(project, features), resource);
    plan.add(
        "tests/api_test.rs",
        render(resource_template!("api_test.rs.tmpl"), &bindings)?,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> ProjectNames {
        ProjectNames::parse("my-notes").unwrap()
    }

    fn resource() -> ResourceNames {
        ResourceNames::parse("note").unwrap()
    }

    #[test]
    fn known_features_parse_and_unknown_ones_are_refused() {
        let features = Features::parse(&["persistence,messaging".to_string()]).unwrap();
        assert!(features.has("persistence"));
        assert!(features.has("messaging"));
        assert!(!features.has("security"));

        let err = Features::parse(&["persistince".to_string()]).unwrap_err();
        assert!(err.to_string().contains("unknown feature"), "{err}");
        assert!(err.to_string().contains("persistence"), "{err}");
    }

    #[test]
    fn the_cargo_feature_list_always_includes_the_runtime() {
        let none = Features::default();
        assert_eq!(none.cargo_list(), "\"server\"");

        let some = Features::parse(&["persistence,telemetry".to_string()]).unwrap();
        assert_eq!(
            some.cargo_list(),
            "\"server\", \"persistence\", \"telemetry-otlp\"",
            "telemetry maps onto the OTLP backend"
        );
    }

    #[test]
    fn a_planned_project_covers_every_ring() {
        let plan = plan_project(
            "/tmp/does-not-matter",
            &project(),
            &resource(),
            &Features::default(),
            "version = \"0.1\"",
        )
        .unwrap();

        let paths: Vec<String> = plan
            .paths()
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        for expected in [
            "Cargo.toml",
            "config.yaml",
            "justfile",
            "Dockerfile",
            "compose.yaml",
            "src/main.rs",
            "src/domain/model/note.rs",
            "src/domain/event/note.rs",
            "src/domain/port/note_repository.rs",
            "src/application/note.rs",
            "src/adapter/inbound/rest/note.rs",
            "src/adapter/outbound/note_repository.rs",
        ] {
            assert!(
                paths.iter().any(|p| p == expected),
                "expected {expected} in {paths:?}"
            );
        }
    }

    #[test]
    fn a_planned_resource_touches_only_resource_files() {
        let plan = plan_resource(
            "/tmp/does-not-matter",
            &project(),
            &ResourceNames::parse("order").unwrap(),
            &Features::default(),
        )
        .unwrap();

        let paths: Vec<String> = plan
            .paths()
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        assert_eq!(paths.len(), 6, "one file per ring: {paths:?}");
        assert!(paths.iter().all(|p| p.contains("order")));
        assert!(!paths.iter().any(|p| p == "Cargo.toml"));
    }

    #[test]
    fn module_declarations_are_inserted_sorted_and_only_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mod.rs");
        std::fs::write(&path, "//! Entities.\n\npub mod note;\npub mod zebra;\n").unwrap();

        append_module_line(&path, "pub mod order;").unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            contents,
            "//! Entities.\n\npub mod note;\npub mod order;\npub mod zebra;\n"
        );

        // Idempotent.
        append_module_line(&path, "pub mod order;").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
    }

    #[test]
    fn a_module_declaration_can_be_added_to_a_file_that_has_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mod.rs");
        std::fs::write(&path, "//! Entities and value objects.\n").unwrap();

        append_module_line(&path, "pub mod note;").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "//! Entities and value objects.\n\npub mod note;\n"
        );
    }

    #[test]
    fn generation_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let mut plan = Plan::new(dir.path());
        plan.add("src/main.rs", "fn main() {}\n".to_string());
        plan.write().unwrap();

        let err = plan.write().unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
    }
}

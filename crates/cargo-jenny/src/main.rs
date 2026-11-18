//! `cargo jenny` — scaffolding for jenny services.
//!
//! New services all have the same skeleton, and the repetitive parts are exactly where
//! the layering gets bent: someone in a hurry puts the DTO next to the entity, or the
//! repository trait next to its sqlx implementation, and the structure the framework
//! relies on quietly stops holding. Generating the right shape by default is a cheaper
//! way to keep it than catching it in review.
//!
//! ```text
//! cargo jenny new my-notes --features persistence,messaging
//! cargo jenny add resource order
//! ```

mod names;
mod scaffold;
mod template;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use names::{ProjectNames, ResourceNames};
use scaffold::{Features, plan_project, plan_project_test, plan_resource};

/// Cargo invokes `cargo-jenny jenny <args>`, so the outer level swallows that first
/// `jenny`. Running the binary directly works too.
#[derive(Parser)]
#[command(name = "cargo-jenny", bin_name = "cargo", version, about)]
enum CargoCli {
    Jenny(Cli),
}

#[derive(Args)]
#[command(version, about = "Scaffolding for jenny services", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a new jenny service.
    New(NewArgs),
    /// Add something to an existing service.
    #[command(subcommand)]
    Add(AddCommand),
}

#[derive(Args)]
struct NewArgs {
    /// Name of the service. May be a path, in which case its last segment is the name.
    name: String,

    /// Optional pieces: persistence, messaging, objects, security, openapi, swagger-ui,
    /// telemetry.
    #[arg(long, value_delimiter = ',')]
    features: Vec<String>,

    /// Name of the starter resource.
    #[arg(long, default_value = "item")]
    resource: String,

    /// Where to create the project. Defaults to `./<name>`.
    #[arg(long)]
    path: Option<PathBuf>,

    /// Depend on a local checkout of jenny instead of the release this CLI belongs to.
    ///
    /// For working on jenny itself: a project generated this way builds against your
    /// working tree.
    #[arg(long)]
    jenny_path: Option<PathBuf>,

    /// Print what would be written, without writing it.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Subcommand)]
enum AddCommand {
    /// Add a resource: aggregate, store port, use cases, REST routes and tests.
    Resource(AddResourceArgs),
}

#[derive(Args)]
struct AddResourceArgs {
    /// Singular name of the resource, e.g. `order`.
    name: String,

    /// The project root. Defaults to the current directory.
    #[arg(long, default_value = ".")]
    path: PathBuf,

    /// Optional pieces the generated code should assume.
    #[arg(long, value_delimiter = ',')]
    features: Vec<String>,

    /// Print what would be written, without writing it.
    #[arg(long)]
    dry_run: bool,
}

fn main() -> Result<()> {
    let CargoCli::Jenny(cli) = CargoCli::parse();
    match cli.command {
        Command::New(args) => new_project(args),
        Command::Add(AddCommand::Resource(args)) => add_resource(args),
    }
}

fn new_project(args: NewArgs) -> Result<()> {
    let project = ProjectNames::parse(&args.name)?;
    let resource = ResourceNames::parse(&args.resource)?;
    let features = Features::parse(&args.features)?;

    let root = args.path.unwrap_or_else(|| PathBuf::from(&project.kebab));
    if root.exists() && root.read_dir().is_ok_and(|mut d| d.next().is_some()) {
        bail!("{} already exists and is not empty", root.display());
    }

    let dependency = match &args.jenny_path {
        Some(path) => {
            let absolute = std::fs::canonicalize(path)
                .with_context(|| format!("resolving --jenny-path {}", path.display()))?;
            let facade = absolute.join("crates/jenny");
            let target = if facade.exists() { facade } else { absolute };
            format!("path = \"{}\"", target.display())
        }
        // jenny's crates are released in lockstep, so the CLI's own version is the
        // jenny version its templates were written against.
        None => format!("version = \"{}\"", env!("CARGO_PKG_VERSION")),
    };

    let mut plan = plan_project(&root, &project, &resource, &features, &dependency)?;
    plan_project_test(&mut plan, &project, &resource, &features)?;

    if args.dry_run {
        report(&plan);
        return Ok(());
    }

    plan.write()?;

    println!("Created {} in {}", project.kebab, plan.root().display());
    report(&plan);
    println!();
    println!("  cd {}", root.display());
    if ["persistence", "messaging", "objects", "telemetry"]
        .iter()
        .any(|feature| features.has(feature))
    {
        println!("  just up      # development dependencies");
    }
    println!("  just test");
    println!("  just run");
    Ok(())
}

fn add_resource(args: AddResourceArgs) -> Result<()> {
    let resource = ResourceNames::parse(&args.name)?;
    let features = Features::parse(&args.features)?;

    let manifest = args.path.join("Cargo.toml");
    if !manifest.exists() {
        bail!(
            "{} does not look like a cargo project (no Cargo.toml)",
            args.path.display()
        );
    }
    let project = project_name_from(&manifest)?;

    let plan = plan_resource(&args.path, &project, &resource, &features)?;
    plan.ensure_clear()?;

    if args.dry_run {
        report(&plan);
        return Ok(());
    }

    plan.write()?;

    println!("Added resource `{}`", resource.snake);
    report(&plan);
    println!();
    println!("Wire it up in src/lib.rs:");
    println!();
    // A project generated with `persistence` says so in its jenny dependency, so the
    // flag need not be repeated here.
    let persistence = features.has("persistence")
        || std::fs::read_to_string(&manifest).is_ok_and(|manifest| {
            manifest
                .lines()
                .any(|line| line.starts_with("jenny =") && line.contains("\"persistence\""))
        });
    println!("{}", wiring(&resource, persistence));
    println!();
    println!(
        "The composition root is the one file worth reading top to bottom, so it is \n\
         left for you to edit rather than rewritten by a generator."
    );
    Ok(())
}

/// The lines `add resource` prints for the composition root: the same Postgres-or-memory
/// choice `new` generated when the project has `persistence`, memory otherwise.
fn wiring(resource: &ResourceNames, persistence: bool) -> String {
    let (snake, plural, pascal, screaming) = (
        &resource.snake,
        &resource.plural,
        &resource.pascal,
        &resource.screaming,
    );
    let store = if persistence {
        format!(
            "    const {screaming}_QUERYABLE: [(&str, ValueKind); 1] = [(\"name\", ValueKind::Text)];\n\
             \x20   let {plural}_store: Arc<domain::port::{snake}_store::{pascal}Store> = match server.postgres()? {{\n\
             \x20       Some(postgres) => Arc::new(postgres.documents::<{pascal}>().queryable({screaming}_QUERYABLE)),\n\
             \x20       None => Arc::new(Memory::<{pascal}>::new().queryable({screaming}_QUERYABLE)),\n\
             \x20   }};"
        )
    } else {
        format!(
            "    let {plural}_store = Arc::new(Memory::<{pascal}>::new().queryable([(\"name\", ValueKind::Text)]));"
        )
    };
    format!(
        "{store}\n\
         \x20   let {plural} = {pascal}s::new({plural}_store, events.clone());\n\
         \n\
         \x20   let rest = rest\n\
         \x20       .mount(adapter::inbound::rest::{snake}::routes({plural}))?;"
    )
}

/// Read `name` out of a `Cargo.toml` without a TOML parser.
///
/// The alternative is a dependency for one field of one file the tool itself generated.
fn project_name_from(manifest: &std::path::Path) -> Result<ProjectNames> {
    let contents = std::fs::read_to_string(manifest)
        .with_context(|| format!("reading {}", manifest.display()))?;

    let name = contents
        .lines()
        .map(str::trim)
        .take_while(|line| !line.starts_with('[') || line.starts_with("[package]"))
        .find_map(|line| line.strip_prefix("name"))
        .and_then(|rest| rest.trim().strip_prefix('='))
        .map(|value| value.trim().trim_matches('"'))
        .filter(|value| !value.is_empty())
        .with_context(|| format!("could not find a package name in {}", manifest.display()))?;

    ProjectNames::parse(name)
}

fn report(plan: &scaffold::Plan) {
    for path in plan.paths() {
        println!("  {}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_printed_wiring_matches_how_the_project_stores_things() {
        let order = ResourceNames::parse("order").unwrap();
        let memory = wiring(&order, false);
        assert!(memory.contains("Memory::<Order>::new()"), "{memory}");
        assert!(!memory.contains("postgres"), "{memory}");

        let postgres = wiring(&order, true);
        assert!(postgres.contains("match server.postgres()?"), "{postgres}");
        assert!(
            postgres.contains("postgres.documents::<Order>()"),
            "{postgres}"
        );
        assert!(postgres.contains("routes(orders)"), "{postgres}");
    }
    use clap::CommandFactory;

    #[test]
    fn the_cargo_subcommand_surface_is_valid() {
        CargoCli::command().debug_assert();
    }

    #[test]
    fn arguments_parse_the_way_cargo_passes_them() {
        let CargoCli::Jenny(cli) = CargoCli::parse_from([
            "cargo",
            "jenny",
            "new",
            "my-notes",
            "--features",
            "persistence,messaging",
        ]);
        let Command::New(args) = cli.command else {
            panic!("expected `new`");
        };
        assert_eq!(args.name, "my-notes");
        assert_eq!(args.features, ["persistence", "messaging"]);
        assert_eq!(args.resource, "item", "the default starter resource");
    }

    #[test]
    fn add_resource_parses() {
        let CargoCli::Jenny(cli) =
            CargoCli::parse_from(["cargo", "jenny", "add", "resource", "order"]);
        let Command::Add(AddCommand::Resource(args)) = cli.command else {
            panic!("expected `add resource`");
        };
        assert_eq!(args.name, "order");
        assert_eq!(args.path, PathBuf::from("."));
    }

    #[test]
    fn the_package_name_is_read_out_of_a_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(
            &manifest,
            "[package]\nname = \"my-notes\"\nversion = \"0.1.0\"\n\n[dependencies]\nname = \"nope\"\n",
        )
        .unwrap();

        let project = project_name_from(&manifest).unwrap();
        assert_eq!(project.kebab, "my-notes");
        assert_eq!(project.snake, "my_notes");
    }

    #[test]
    fn a_manifest_without_a_name_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(&manifest, "[package]\nversion = \"0.1.0\"\n").unwrap();
        assert!(project_name_from(&manifest).is_err());
    }
}

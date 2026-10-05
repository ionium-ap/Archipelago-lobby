use anyhow::{Context, Result};
use apwm::changes;
use apwm::lint::Severity;
use apwm::utils::git_clone_shallow;
use clap::Parser;
use semver::Version;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

#[derive(clap::Subcommand)]
enum Command {
    Update {
        #[clap(short)]
        index_path: PathBuf,
    },
    Download {
        #[clap(short)]
        index_path: PathBuf,
        #[clap(short)]
        destination: PathBuf,
        #[clap(short, conflicts_with = "from_changes")]
        precise: Option<String>,
        #[clap(long, conflicts_with = "precise")]
        from_changes: Option<PathBuf>,
    },
    Install {
        #[clap(short)]
        index_path: PathBuf,
        #[clap(short)]
        apworlds_path: PathBuf,
        #[clap(short)]
        destination: PathBuf,
        #[clap(short)]
        precise: Option<String>,
        /// The Archipelago base to install the latest releases of. Defaults to the index's
        /// `archipelago_version`
        #[clap(long)]
        base: Option<Version>,
    },
    Changes {
        #[clap(short)]
        index_path: PathBuf,
        #[clap(short)]
        from: String,
        #[clap(short = 'r')]
        from_ref: Option<String>,
        #[clap(short)]
        output: PathBuf,
    },
    /// Checks an index for what would break lobbies that read it. Exits with 1 on any error
    Lint {
        #[clap(short)]
        index_path: PathBuf,
    },
}

#[derive(clap::Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let cli = Args::parse();
    match cli.command {
        Command::Update { index_path } => {
            update(&index_path).await?;
        }
        Command::Download {
            index_path,
            destination,
            precise,
            from_changes,
        } => {
            download(
                &index_path,
                &destination,
                precise.as_deref(),
                from_changes.as_deref(),
            )
            .await?;
        }
        Command::Install {
            index_path,
            apworlds_path,
            destination,
            precise,
            base,
        } => {
            install(
                &index_path,
                &apworlds_path,
                &destination,
                precise.as_deref(),
                base.as_ref(),
            )
            .await?;
        }
        Command::Changes {
            index_path,
            from,
            from_ref,
            output,
        } => {
            compute_changes(&index_path, &from, from_ref.as_deref(), &output).await?;
        }
        Command::Lint { index_path } => {
            if !lint(&index_path)? {
                std::process::exit(1);
            }
        }
    }

    Ok(())
}

/// Prints what the linter found and returns whether the index is free of errors.
fn lint(index_path: &Path) -> Result<bool> {
    let problems = apwm::lint::lint(&index_path.join("index.toml"))?;
    for problem in &problems {
        println!("{problem}");
    }

    let errors = problems
        .iter()
        .filter(|problem| problem.severity == Severity::Error)
        .count();
    println!("{errors} error(s), {} warning(s)", problems.len() - errors);

    Ok(errors == 0)
}

async fn download(
    index_path: &Path,
    destination: &Path,
    precise: Option<&str>,
    from_changes: Option<&Path>,
) -> Result<()> {
    let index_toml = index_path.join("index.toml");
    let index_set = apwm::IndexSet::new(&index_toml)?;

    if let Some(changes_path) = from_changes {
        let content = std::fs::read_to_string(changes_path)
            .with_context(|| format!("Reading changes file: {}", changes_path.display()))?;
        let changes_file: changes::Changes = serde_json::from_str(&content)?;
        changes::download_from_changes(&changes_file, &index_set.all_releases(), destination)
            .await?;
    } else {
        let target = apworld_version_from_precise(precise)?;
        index_set.refresh_into(destination, false, target).await?;
    }

    Ok(())
}

async fn update(index_path: &Path) -> Result<()> {
    let index_toml = index_path.join("index.toml");
    let index_set = apwm::IndexSet::new(&index_toml)?;
    let destination = tempdir()?;

    let new_lock = index_set
        .refresh_into(destination.path(), true, None)
        .await?;

    new_lock.write()?;

    Ok(())
}

fn apworld_version_from_precise(precise: Option<&str>) -> Result<Option<(String, Version)>> {
    if let Some(precise) = precise {
        let parts = precise.splitn(2, ':').collect::<Vec<_>>();
        if parts.len() != 2 {
            anyhow::bail!("Precise version need to be of the form <apworld>:<version>");
        }

        Ok(Some((parts[0].to_string(), parts[1].parse::<Version>()?)))
    } else {
        Ok(None)
    }
}

async fn install(
    index_path: &Path,
    apworlds_path: &Path,
    destination: &Path,
    precise: Option<&str>,
    base: Option<&Version>,
) -> Result<()> {
    let index_toml = index_path.join("index.toml");
    let index_set = apwm::IndexSet::new(&index_toml)?;
    let index = match base {
        Some(base) => index_set
            .get(base)
            .with_context(|| format!("The index doesn't describe Archipelago {base}"))?,
        None => index_set.legacy(),
    };

    std::fs::create_dir_all(destination).context("While creating destination dir")?;
    let target = apworld_version_from_precise(precise)?;

    for (world_name, world) in &index.worlds {
        let version = if let Some((ref target_apworld, ref target_version)) = target {
            if target_apworld != world_name {
                continue;
            }
            target_version
        } else {
            let Some((version, origin)) = world.get_latest_release() else {
                continue;
            };
            // Core worlds ship with Archipelago, there's no file to install for them
            if origin.is_supported() {
                continue;
            }
            version
        };

        let apworld_path = index.get_world_local_path(apworlds_path, world_name, version);
        let destination = destination.join(format!("{}.apworld", world_name));
        std::fs::copy(&apworld_path, &destination)
            .with_context(|| format!("Cannot copy {:?} to {:?}", &apworld_path, &destination))?;
    }

    Ok(())
}

async fn compute_changes(
    index_path: &Path,
    from_git_remote: &str,
    from_git_ref: Option<&str>,
    output: &Path,
) -> Result<()> {
    let old_index_dir = tempdir()?;
    git_clone_shallow(
        from_git_remote,
        from_git_ref.unwrap_or("main"),
        old_index_dir.path(),
    )?;

    let new_index_toml = index_path.join("index.toml");
    let old_index_toml = old_index_dir.path().join("index.toml");

    let new_index = apwm::IndexSet::new(&new_index_toml)?;
    let old_index = apwm::IndexSet::new(&old_index_toml)?;

    let mut result = changes::compute_changes_between(&old_index, &new_index)?;

    std::fs::create_dir_all(output)?;
    let checksums =
        changes::download_changed_apworlds(&result, &new_index.all_releases(), output).await?;
    changes::apply_checksums(&mut result, checksums);
    changes::write_changes(&result, output)?;

    Ok(())
}

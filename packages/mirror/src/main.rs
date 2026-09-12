//! `mirror`: opt-in standalone GitHub repos generated from this monorepo.
//!
//! A package with a `mirror` attr in its `package.nix` gets a self-contained
//! source tree (inlined workspace inheritance, pruned `Cargo.lock`, banner
//! README) snapshot-synced into a read-only GitHub mirror repo (`gen` /
//! `publish`). The monorepo stays the source of truth. De-forked packages
//! are NOT this tool's product. They live as jj views (`views.toml`) here.
//!
//! CI drives this from `.github/workflows/mirror-sync.yml`.

mod changelog;
mod exec;
mod generate;
mod lockfile;
mod manifest;
mod mirrors;
mod publish;
mod readme;
mod workspace;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::workspace::Workspace;

/// The `owner/name` of the monorepo every mirror points back at.
pub const MONOREPO_SLUG: &str = "indexable-inc/index";

#[derive(Parser)]
#[command(
    name = "mirror",
    about = "Generate and sync standalone repos for opt-in packages"
)]
struct Cli {
    /// Monorepo root (defaults to the nearest ancestor with a `[workspace]`).
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a package's standalone source tree into a directory.
    Gen {
        /// Repo-relative package path, e.g. `packages/progress-style`.
        #[arg(long)]
        package: PathBuf,
        /// Output directory (created; must be empty when it exists).
        #[arg(long)]
        out: PathBuf,
        /// Mirror repo `owner/name`, named in the generated README banner.
        #[arg(long)]
        repo: Option<String>,
        /// Mirror manifest as a JSON file (the rendered `.#lib.mirrorPackages`
        /// list), sourcing the repo, description, and flake attr for the
        /// generated README; without it the README falls back to the crate's
        /// own metadata (`publish` always resolves the manifest itself).
        #[arg(long)]
        mirror_json: Option<PathBuf>,
    },
    /// Generate a package's tree and snapshot-sync it into its mirror repo.
    Publish {
        /// Repo-relative package path, e.g. `packages/progress-style`.
        #[arg(long)]
        package: PathBuf,
        /// Push target URL (overrides the repo from the mirror manifest).
        #[arg(long)]
        remote_url: Option<String>,
        /// Mirror repo `owner/name` (overrides the mirror manifest).
        #[arg(long)]
        repo: Option<String>,
        /// Create the GitHub repo (via `gh repo create`) when it is missing.
        #[arg(long)]
        create: bool,
        /// Mirror manifest as a JSON file (the rendered `.#lib.mirrorPackages`
        /// list); defaults to evaluating it with `nix eval --json`.
        #[arg(long)]
        mirror_json: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let workspace = Workspace::locate(cli.root.as_deref())?;
    match cli.command {
        Command::Gen {
            package,
            out,
            repo,
            mirror_json,
        } => {
            let entry = mirror_json
                .as_deref()
                .map(|json| mirrors::entry_for(&workspace, &package, Some(json)))
                .transpose()?;
            let generated = generate::run(
                &workspace,
                &generate::Request {
                    package: &package,
                    out: &out,
                    mirror_repo: repo
                        .as_deref()
                        .or_else(|| entry.as_ref().map(|entry| entry.repo.as_str())),
                    description: entry
                        .as_ref()
                        .and_then(|entry| entry.description.as_deref()),
                    flake_attr: entry.as_ref().and_then(|entry| entry.flake_attr.as_deref()),
                },
            )?;
            println!(
                "generated `{}` into {} ({} internal dependencies)",
                generated.crate_name,
                out.display(),
                generated.internal.len()
            );
            Ok(())
        }
        Command::Publish {
            package,
            remote_url,
            repo,
            create,
            mirror_json,
        } => publish::run(
            &workspace,
            &publish::Request {
                package,
                remote_url,
                repo,
                create,
                mirror_json,
            },
        ),
    }
}

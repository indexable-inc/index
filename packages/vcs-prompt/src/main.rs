//! `vcs-prompt`: the version-control segment of the starship prompt, for jj
//! workspaces as well as git repositories.
//!
//! Starship's built-in `git_*` modules cannot be disabled per directory, and
//! inside a colocated jj repo they describe the exported git view: a detached
//! HEAD sitting wherever `jj git export` last wrote, with none of the working
//! copy's real state. So the config turns them off and calls this instead,
//! which picks the VCS by walking up from the prompt directory and renders one
//! segment for whichever it finds.
//!
//! ```text
//! $ vcs-prompt            # colocated jj fork checkout
//! on 󱗆 lsurukvy ix-patched+2 *
//! $ vcs-prompt            # plain git checkout
//! on  main !3?1⇡2
//! $ vcs-prompt age        # either, for the commit-age segment
//! 13 minutes ago
//! ```

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use color_eyre::eyre::{Result, WrapErr};

mod age;
mod git;
mod jj;
mod render;
mod views;
mod workspace;

use workspace::Workspace;

#[derive(Parser)]
#[command(
    version,
    about = "Starship VCS segment: jj working-copy state in a jj workspace, git branch and status elsewhere"
)]
struct Cli {
    /// Directory to describe. Defaults to the working directory, which is the
    /// directory starship is rendering the prompt for.
    #[arg(long, global = true)]
    cwd: Option<PathBuf>,
    /// Print the segment without ANSI escapes.
    #[arg(long, global = true)]
    no_color: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Exit 0 inside a jj or git workspace, 1 outside one. This is the
    /// `when` gate for the custom module: it only stats directories, so it
    /// costs nothing next to rendering.
    Detect,
    /// Print how long ago the latest commit landed, e.g. `13 minutes ago`.
    /// Exits 1 with no output when there is no commit to date, so the
    /// segment renders empty rather than wrong.
    Age,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let cwd = match working_directory(&cli) {
        Ok(cwd) => cwd,
        Err(error) => return report(&error),
    };
    let Some(workspace) = workspace::discover(&cwd) else {
        // Outside a workspace there is no segment. Starship reads the
        // non-zero exit as "render nothing", which is both what `detect`
        // promises and what an unversioned directory should show.
        return ExitCode::FAILURE;
    };

    if matches!(cli.command, Some(Command::Detect)) {
        return ExitCode::SUCCESS;
    }

    if matches!(cli.command, Some(Command::Age)) {
        return match age::since_last_commit(&workspace) {
            Ok(Some(age)) => {
                println!("{age}");
                ExitCode::SUCCESS
            }
            Ok(None) => ExitCode::FAILURE,
            Err(error) => report(&error),
        };
    }

    match segment(&workspace, &cwd, !cli.no_color) {
        Ok(segment) => {
            println!("{segment}");
            ExitCode::SUCCESS
        }
        Err(error) => report(&error),
    }
}

fn working_directory(cli: &Cli) -> Result<PathBuf> {
    cli.cwd.as_ref().map_or_else(
        || env::current_dir().wrap_err("failed to read the working directory"),
        |cwd| Ok(cwd.clone()),
    )
}

fn segment(workspace: &Workspace, cwd: &Path, color: bool) -> Result<String> {
    Ok(match workspace {
        Workspace::Jj(root) => {
            // Two independent jj invocations; overlapped, the segment costs
            // the slower one (~30ms with a release jj) instead of their sum.
            let (head, view) = std::thread::scope(|scope| {
                let view = scope.spawn(|| views::at(root, cwd));
                (jj::head(root), view.join().expect("the views thread"))
            });
            // A failed view lookup is rendered, never dropped: the report goes
            // to stderr (starship logs it) and the segment shows `view:ERR`.
            let view = match view {
                Ok(Some(view)) => views::Segment::Inside(view),
                Ok(None) => views::Segment::Outside,
                Err(error) => {
                    eprintln!("vcs-prompt: {error:#}");
                    views::Segment::Failed
                }
            };
            render::jj(&head?, &view, color)
        }
        Workspace::Git(root) => render::git(&git::head(root)?, color),
    })
}

/// A prompt cannot pop up a backtrace: the report is one line on stderr, which
/// starship logs, and an empty segment.
fn report(error: &color_eyre::Report) -> ExitCode {
    eprintln!("vcs-prompt: {error:#}");
    ExitCode::FAILURE
}

use anyhow::{Context, Result};
use argp::FromArgs;
use tracing::{info, warn};
use typed_path::Utf8NativePathBuf;

use crate::util::{
    path::native_path,
    renames::{RenameReport, Renames, apply_renames},
};

#[derive(FromArgs, PartialEq, Debug)]
/// Commands for working with symbols files.
#[argp(subcommand, name = "symbols")]
pub struct Args {
    #[argp(subcommand)]
    command: SubCommand,
}

#[derive(FromArgs, PartialEq, Debug)]
#[argp(subcommand)]
enum SubCommand {
    Rename(RenameArgs),
}

#[derive(FromArgs, PartialEq, Eq, Debug)]
/// Applies `old_name = new_name` pairs to a symbols file.
#[argp(subcommand, name = "rename")]
pub struct RenameArgs {
    #[argp(positional, from_str_fn(native_path))]
    /// symbols file to rewrite, in place
    symbols: Utf8NativePathBuf,
    #[argp(positional, from_str_fn(native_path))]
    /// rename file, as written by `dtk match`
    renames: Utf8NativePathBuf,
    #[argp(switch, short = 'n')]
    /// report what would change without writing anything
    dry_run: bool,
}

pub fn run(args: Args) -> Result<()> {
    match args.command {
        SubCommand::Rename(c_args) => rename(c_args),
    }
}

fn rename(args: RenameArgs) -> Result<()> {
    let renames = Renames::read(&args.renames)?;
    info!("Read {} renames from {}", renames.len(), args.renames);

    let report = apply_renames(&args.symbols, &renames, args.dry_run)
        .with_context(|| format!("While applying renames to {}", args.symbols))?;
    report_outcome(&report, &args.symbols, args.dry_run);
    Ok(())
}

pub fn report_outcome(report: &RenameReport, symbols: &Utf8NativePathBuf, dry_run: bool) {
    if dry_run {
        info!("Would rename {} symbols in {}", report.applied, symbols);
    } else {
        info!("Renamed {} symbols in {}", report.applied, symbols);
    }

    if !report.missing.is_empty() {
        warn!(
            "{} names were not found in {} (first: {})",
            report.missing.len(),
            symbols,
            report.missing[0]
        );
    }
    // A collision means the new name is already held by a symbol that isn't
    // being renamed away, so applying it would leave two symbols with one name.
    // Skipping keeps the file unambiguous, which is the point of the exercise.
    for (from, to) in &report.collisions {
        warn!("Skipped {from} -> {to}: '{to}' is already taken by another symbol");
    }
    if !report.collisions.is_empty() {
        warn!("{} renames skipped due to name collisions", report.collisions.len());
    }
}

use anyhow::{Context, Result};
use argp::FromArgs;
use tracing::{info, warn};
use typed_path::Utf8NativePathBuf;

use crate::util::{path::native_path, split_merge::merge_splits};

#[derive(FromArgs, PartialEq, Debug)]
/// Commands for working with splits files.
#[argp(subcommand, name = "splits")]
pub struct Args {
    #[argp(subcommand)]
    command: SubCommand,
}

#[derive(FromArgs, PartialEq, Debug)]
#[argp(subcommand)]
enum SubCommand {
    Merge(MergeArgs),
}

#[derive(FromArgs, PartialEq, Eq, Debug)]
/// Merges the confident units from a `dtk match --splits` proposal into a
/// splits file, in place.
#[argp(subcommand, name = "merge")]
pub struct MergeArgs {
    #[argp(positional, from_str_fn(native_path))]
    /// splits file to update, in place
    splits: Utf8NativePathBuf,
    #[argp(positional, from_str_fn(native_path))]
    /// proposal file, as written by `dtk match --splits`
    proposal: Utf8NativePathBuf,
    #[argp(switch, short = 'n')]
    /// report what would change without writing anything
    dry_run: bool,
}

pub fn run(args: Args) -> Result<()> {
    match args.command {
        SubCommand::Merge(c_args) => merge(c_args),
    }
}

fn merge(args: MergeArgs) -> Result<()> {
    let report = merge_splits(&args.splits, &args.proposal, args.dry_run)
        .with_context(|| format!("While merging {} into {}", args.proposal, args.splits))?;

    if args.dry_run {
        info!("Would add {} units to {}", report.added.len(), args.splits);
    } else {
        info!("Added {} units to {}", report.added.len(), args.splits);
    }
    if !report.already_present.is_empty() {
        warn!(
            "{} proposed units already have a split, left untouched (first: {})",
            report.already_present.len(),
            report.already_present[0]
        );
    }
    Ok(())
}

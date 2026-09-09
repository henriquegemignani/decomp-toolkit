use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    io::Write,
};

use anyhow::{Context, Result, anyhow, bail};
use argp::FromArgs;
use serde::Serialize;
use tracing::info;
use typed_path::{Utf8NativePath, Utf8NativePathBuf};

use crate::{
    analysis::{
        coverage::build_report as build_coverage_report,
        data_matching::{DataMatch, match_data},
        matching::{MatchOptions, MatchResult, MatchTarget, MatchTier, match_functions},
        tracker::Tracker,
        unit_matching::{UnitProposal, UnitTier, propose_units},
    },
    cmd::dol::{ObjectBase, ProjectConfig, find_object_base, load_analyze_dol},
    obj::ObjInfo,
    util::{
        file::buf_writer,
        path::{check_path_buf, native_path},
    },
    vfs::open_file,
};

#[derive(FromArgs, PartialEq, Debug)]
/// Matches functions between two versions of the same executable.
#[argp(subcommand, name = "match")]
pub struct Args {
    #[argp(positional, from_str_fn(native_path))]
    /// source project configuration (the version with known symbol names)
    source: Utf8NativePathBuf,
    #[argp(positional, from_str_fn(native_path))]
    /// target project configuration (the version to name)
    target: Utf8NativePathBuf,
    #[argp(option, short = 'o', from_str_fn(native_path))]
    /// write the full JSON report to this path
    output: Option<Utf8NativePathBuf>,
    #[argp(option, short = 'r', from_str_fn(native_path))]
    /// write confident `target_name = source_name` rename pairs to this path.
    /// Only matches safe to apply unreviewed are included
    renames: Option<Utf8NativePathBuf>,
    #[argp(option, from_str_fn(native_path))]
    /// write everything short of confident to this path, with alternatives, for
    /// review. Never mixed into --renames
    candidates: Option<Utf8NativePathBuf>,
    #[argp(option, from_str_fn(native_path))]
    /// write proposed split-unit boundaries to this path, grouped by unit and
    /// in splits.txt syntax. Confident entries are ready to paste in;
    /// candidates are commented out with the reason they weren't confident
    splits: Option<Utf8NativePathBuf>,
    #[argp(option, from_str_fn(native_path))]
    /// write evidence for conservative partial translation-unit coverage
    coverage: Option<Utf8NativePathBuf>,
    #[argp(option, short = 'c')]
    /// minimum confidence for a match to be reported (default 0.5)
    min_confidence: Option<f32>,
    #[argp(option)]
    /// cap on propagation rounds (default 100). Propagation stops on its own
    /// once a round finds nothing; raise this only if it reports hitting the cap
    max_rounds: Option<u32>,
    #[argp(option, from_str_fn(native_path))]
    /// project root the source configuration's relative paths resolve against
    /// (default: the working directory, else inferred from the config location)
    source_root: Option<Utf8NativePathBuf>,
    #[argp(option, from_str_fn(native_path))]
    /// same, for the target configuration
    target_root: Option<Utf8NativePathBuf>,
    #[argp(switch)]
    /// ignore existing names while matching, then score the result against
    /// them; use on an already-named version to measure accuracy
    validate: bool,
}

pub fn run(args: Args) -> Result<()> {
    let mut options = MatchOptions {
        min_confidence: args.min_confidence.unwrap_or(0.5),
        ignore_names: args.validate,
        ..Default::default()
    };
    if let Some(max_rounds) = args.max_rounds {
        options.max_rounds = max_rounds;
    }

    let source_obj = load_analyzed(&args.source, args.source_root.as_deref(), "--source-root")
        .with_context(|| format!("While loading {}", args.source))?;
    let target_obj = load_analyzed(&args.target, args.target_root.as_deref(), "--target-root")
        .with_context(|| format!("While loading {}", args.target))?;

    let source = MatchTarget::new(args.source.to_string(), source_obj);
    let target = MatchTarget::new(args.target.to_string(), target_obj);
    info!("Matching {} functions against {} functions", source.graph.len(), target.graph.len());

    let result = match_functions(&source, &target, &options);
    let data_matches = match_data(&source, &target, &result);
    let report = Report::build(&source, &target, &result, args.validate);
    report.print_summary();

    if let Some(path) = &args.output {
        let mut file = buf_writer(path)?;
        serde_json::to_writer_pretty(&mut file, &report)?;
        file.flush()?;
        info!("Wrote report to {}", path);
    }
    if let Some(path) = &args.renames {
        let mut file = buf_writer(path)?;
        let mut count = 0;
        for m in report.renameable().filter(|m| m.tier == MatchTier::Confident) {
            write!(file, "{} = {}", m.target_name, m.source_name)?;
            if m.source_local {
                write!(file, " local")?;
            }
            writeln!(file)?;
            count += 1;
        }
        let mut data_count = 0;
        for dm in renameable_data(&source, &target, &data_matches) {
            write!(
                file,
                "{} = {}",
                target.symbol_name_at(dm.target),
                source.symbol_name_at(dm.source)
            )?;
            if source.is_local_at(dm.source) {
                write!(file, " local")?;
            }
            writeln!(file)?;
            data_count += 1;
        }
        file.flush()?;
        info!("Wrote {} confident renames ({} data) to {}", count + data_count, data_count, path);
    }
    if let Some(path) = &args.candidates {
        let mut file = buf_writer(path)?;
        writeln!(file, "# Candidate names: {} -> {}", report.source, report.target)?;
        writeln!(file, "#")?;
        writeln!(
            file,
            "# These are NOT confident enough to apply unreviewed. Move a line into the"
        )?;
        writeln!(
            file,
            "# renames file once you've confirmed it; delete it otherwise. Lines starting"
        )?;
        writeln!(file, "# with '#' are alternatives that lost, kept so you can see what else it")?;
        writeln!(file, "# could have been.")?;
        writeln!(file)?;

        // Strongest tier first, then by confidence, so a reviewer working top to
        // bottom hits the most likely names first and can stop when quality drops.
        let mut pending: Vec<&ReportMatch> =
            report.renameable().filter(|m| m.tier != MatchTier::Confident).collect();
        pending.sort_by(|a, b| {
            a.tier
                .cmp(&b.tier)
                .then(b.confidence.total_cmp(&a.confidence))
                .then(a.target_address.cmp(&b.target_address))
        });

        let mut count = 0;
        for m in pending {
            let local = if m.source_local { " local" } else { "" };
            writeln!(
                file,
                "{} = {}{local}  # {} {:.2} {}",
                m.target_name,
                m.source_name,
                m.tier.as_str(),
                m.confidence,
                m.method
            )?;
            if let Some(alternative) = &m.alternative {
                writeln!(
                    file,
                    "#{:>width$} = {}  # alternative, {:.0}% as strong",
                    "alt",
                    alternative.name,
                    alternative.relative_score * 100.0,
                    width = m.target_name.len().saturating_sub(1)
                )?;
            }
            count += 1;
        }
        file.flush()?;
        info!("Wrote {} candidates to {}", count, path);
    }
    if let Some(path) = &args.splits {
        let proposals = propose_units(&source, &target, &result, &data_matches);
        write_unit_proposals(path, &target, &proposals)?;
    }
    if let Some(path) = &args.coverage {
        let coverage = build_coverage_report(&source, &target, args.validate);
        let mut file = buf_writer(path)?;
        serde_json::to_writer_pretty(&mut file, &coverage)?;
        file.flush()?;
        info!("Wrote coverage evidence to {}", path);
    }
    Ok(())
}

/// Data matches that would give a currently-unnamed target symbol a name.
fn renameable_data<'a>(
    source: &MatchTarget,
    target: &MatchTarget,
    data_matches: &'a [DataMatch],
) -> Vec<&'a DataMatch> {
    data_matches
        .iter()
        .filter(|dm| source.is_named_at(dm.source) && !target.is_named_at(dm.target))
        .collect()
}

/// Writes proposed split boundaries in splits.txt syntax, grouped by unit.
/// Confident entries are real lines; candidates are commented out with the
/// reason they weren't confident, so the file is ready to paste from once
/// reviewed.
fn write_unit_proposals(
    path: &Utf8NativePath,
    target: &MatchTarget,
    proposals: &[UnitProposal],
) -> Result<()> {
    let mut file = buf_writer(path)?;
    writeln!(file, "# Proposed split boundaries, derived from function matches.")?;
    writeln!(file, "#")?;
    writeln!(file, "# Confident entries are ready to paste into a splits.txt. Candidates are")?;
    writeln!(file, "# commented out, with the reason they weren't confident; review before")?;
    writeln!(file, "# uncommenting.")?;
    writeln!(file)?;

    let mut by_unit: BTreeMap<&str, Vec<&UnitProposal>> = BTreeMap::new();
    for p in proposals {
        by_unit.entry(p.unit.as_str()).or_default().push(p);
    }

    let (mut confident, mut candidates) = (0, 0);
    for (unit, entries) in by_unit {
        writeln!(file, "{unit}:")?;
        for p in entries {
            let section_name =
                target.obj.sections.get(p.section).map(|s| s.name.as_str()).unwrap_or("?");
            let line =
                format!("\t{:<11} start:{:#010X} end:{:#010X}", section_name, p.start, p.end);
            match p.tier {
                UnitTier::Confident => {
                    writeln!(file, "{line}")?;
                    confident += 1;
                }
                UnitTier::Candidate => {
                    writeln!(file, "#{line}  # candidate: {}", p.reasons.join("; "))?;
                    candidates += 1;
                }
            }
        }
        writeln!(file)?;
    }
    file.flush()?;
    info!(
        "Wrote {} confident and {} candidate split boundaries to {}",
        confident, candidates, path
    );
    Ok(())
}

/// Runs the standard DOL analysis pipeline, up to and including relocation
/// tracking, which is what populates the call edges the matcher relies on.
fn load_analyzed(
    config_path: &Utf8NativePath,
    root: Option<&Utf8NativePath>,
    root_option: &str,
) -> Result<ObjInfo> {
    let config: ProjectConfig = {
        let mut file = open_file(config_path, true)?;
        serde_yaml::from_reader(file.as_mut())?
    };
    // Must be resolved before entering the root, since it probes relative paths
    // against the working directory.
    let root = resolve_project_root(&config, config_path, root, root_option)?;
    let _guard = root.map(WorkingDirectory::enter).transpose()?;

    let object_base: ObjectBase = find_object_base(&config)?;
    let mut obj = load_analyze_dol(&config, &object_base)?.obj;

    let mut tracker = Tracker::new(&obj);
    tracker.process(&obj)?;
    tracker.apply(&mut obj, false)?;
    Ok(obj)
}

/// Finds the directory a configuration's relative paths are written against.
///
/// Every other dtk command resolves these against the working directory, which
/// is fine when a command operates on a single project. `match` takes two, and
/// they may live in separate repositories, so `-C` can't serve both. Returns
/// `None` when the working directory already works, leaving behavior untouched.
fn resolve_project_root(
    config: &ProjectConfig,
    config_path: &Utf8NativePath,
    override_root: Option<&Utf8NativePath>,
    root_option: &str,
) -> Result<Option<Utf8NativePathBuf>> {
    if let Some(root) = override_root {
        return Ok(Some(root.to_path_buf()));
    }
    // A path the configuration names that must exist under the correct root.
    let probe =
        config.object_base.clone().unwrap_or_else(|| config.base.object.clone()).with_encoding();
    if fs::metadata(&probe).is_ok() {
        return Ok(None);
    }
    // Walk up from the configuration itself, so an absolute path to a config in
    // another project resolves without having to change directory first.
    let mut directory = config_path.parent();
    while let Some(current) = directory {
        if fs::metadata(current.join(&probe)).is_ok() {
            return Ok(Some(current.to_path_buf()));
        }
        directory = current.parent();
    }
    bail!(
        "Couldn't locate '{probe}' from the working directory or anywhere above {config_path}.\n\
         Run from the project root, or pass {root_option} <dir>."
    )
}

/// Enters a directory, restoring the previous one when dropped.
///
/// A project configuration's paths are written relative to its own root, and the
/// rest of dtk resolves them against the working directory. Rather than rewriting
/// every path a configuration can carry, load each project from where its paths
/// were meant to be read. Loads are sequential; this would need revisiting if
/// they were ever run in parallel.
struct WorkingDirectory(Utf8NativePathBuf);

impl WorkingDirectory {
    fn enter(path: Utf8NativePathBuf) -> Result<Self> {
        let previous = check_path_buf(env::current_dir()?)
            .map_err(|e| anyhow!("Working directory is not valid UTF-8: {e}"))?;
        env::set_current_dir(&path)
            .with_context(|| format!("Failed to change working directory to '{path}'"))?;
        Ok(Self(previous))
    }
}

impl Drop for WorkingDirectory {
    fn drop(&mut self) {
        if let Err(e) = env::set_current_dir(&self.0) {
            log::warn!("Failed to restore working directory '{}': {e}", self.0);
        }
    }
}

#[derive(Serialize)]
struct Report {
    source: String,
    target: String,
    summary: Summary,
    matches: Vec<ReportMatch>,
    unmatched_source: Vec<String>,
    unmatched_target: Vec<String>,
}

#[derive(Serialize)]
struct Summary {
    source_functions: usize,
    target_functions: usize,
    matched: usize,
    /// Matches that would give a currently-unnamed target function a name.
    renames: usize,
    /// Of those, the ones safe to apply without review.
    confident_renames: usize,
    /// Of those, the ones needing review before use.
    candidate_renames: usize,
    by_tier: Vec<(String, usize)>,
    by_method: Vec<(String, usize)>,
    rounds: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    validation: Option<Validation>,
}

/// Accuracy measured against target functions whose real name is already known.
#[derive(Serialize)]
struct Validation {
    checked: usize,
    correct: usize,
    incorrect: usize,
    precision: f32,
    /// Named target functions the matcher left unmatched.
    missed: usize,
    recall: f32,
    /// The same numbers split by tier. `confident` is the one that has to hold
    /// up, since it's the only tier applied without review.
    by_tier: Vec<TierAccuracy>,
}

#[derive(Serialize)]
struct TierAccuracy {
    tier: String,
    checked: usize,
    correct: usize,
    incorrect: usize,
    precision: f32,
}

#[derive(Serialize)]
struct ReportMatch {
    source_name: String,
    /// Whether the source symbol is marked local scope, e.g. a per-translation-unit
    /// template instantiation. Carried onto a rename so the target gets the
    /// same scope, rather than every duplicate ending up global.
    source_local: bool,
    #[serde(serialize_with = "hex")]
    source_address: u32,
    target_name: String,
    #[serde(serialize_with = "hex")]
    target_address: u32,
    #[serde(serialize_with = "tier_name")]
    tier: MatchTier,
    method: &'static str,
    confidence: f32,
    evidence: u32,
    round: u32,
    /// The strongest name that lost to this one, when something competed.
    #[serde(skip_serializing_if = "Option::is_none")]
    alternative: Option<ReportAlternative>,
    /// Whether applying this match would name a previously-unnamed function.
    #[serde(skip)]
    renames: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    verdict: Option<&'static str>,
}

#[derive(Serialize)]
struct ReportAlternative {
    name: String,
    relative_score: f32,
}

fn hex<S>(value: &u32, serializer: S) -> Result<S::Ok, S::Error>
where S: serde::Serializer {
    serializer.serialize_str(&format!("{value:#010X}"))
}

impl Report {
    fn build(
        source: &MatchTarget,
        target: &MatchTarget,
        result: &MatchResult,
        validate: bool,
    ) -> Self {
        let mut matches = Vec::with_capacity(result.matches.len());
        for m in &result.matches {
            let source_name = source.symbol_name(m.source).to_string();
            let target_name = target.symbol_name(m.target).to_string();
            let will_rename = source.is_named(m.source) && !target.is_named(m.target);

            // Only target functions that already carry a real name can be
            // scored, and only then against a source that also has one.
            let verdict = if validate && source.is_named(m.source) && target.is_named(m.target) {
                Some(if source_name == target_name { "correct" } else { "incorrect" })
            } else {
                None
            };

            matches.push(ReportMatch {
                source_name,
                source_local: source.is_local(m.source),
                source_address: source.graph.node(m.source).address,
                target_name,
                target_address: target.graph.node(m.target).address,
                tier: m.tier(),
                method: m.method.as_str(),
                confidence: (m.confidence * 1000.0).round() / 1000.0,
                evidence: m.evidence,
                round: m.round,
                alternative: m.runner_up.map(|a| ReportAlternative {
                    name: source.symbol_name(a.source).to_string(),
                    relative_score: (a.relative_score * 1000.0).round() / 1000.0,
                }),
                renames: will_rename,
                verdict,
            });
        }
        matches.sort_by(|a, b| {
            b.confidence.total_cmp(&a.confidence).then(a.target_address.cmp(&b.target_address))
        });

        let unmatched_source = unmatched(source, &result.source_to_target);
        let unmatched_target = unmatched(target, &result.target_to_source);

        // Tallied from the finished rows rather than accumulated alongside
        // them, so there's one source of truth for what a match's tier and
        // rename status are.
        let mut tier_counts: BTreeMap<MatchTier, usize> = BTreeMap::new();
        let mut method_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut rename_tiers: BTreeMap<MatchTier, usize> = BTreeMap::new();
        for m in &matches {
            *tier_counts.entry(m.tier).or_default() += 1;
            *method_counts.entry(m.method).or_default() += 1;
            if m.renames {
                *rename_tiers.entry(m.tier).or_default() += 1;
            }
        }
        let renames: usize = rename_tiers.values().sum();

        let validation = validate.then(|| {
            let checked = matches.iter().filter(|m| m.verdict.is_some()).count();
            let correct = matches.iter().filter(|m| m.verdict == Some("correct")).count();

            // A named target function left unmatched is a miss only if the source
            // actually had that name to give.
            let source_names: HashSet<&str> = (0..source.graph.len() as u32)
                .filter(|&node| source.is_named(node))
                .map(|node| source.symbol_name(node))
                .collect();
            let missed = result
                .target_to_source
                .iter()
                .enumerate()
                .filter(|(node, matched)| {
                    let node = *node as u32;
                    matched.is_none()
                        && target.is_named(node)
                        && source_names.contains(target.symbol_name(node))
                })
                .count();

            // Scored per tier, so the tier boundaries can be justified from
            // data rather than asserted.
            let mut tier_scores: BTreeMap<MatchTier, (usize, usize)> = BTreeMap::new();
            for m in &matches {
                let Some(verdict) = m.verdict else { continue };
                let entry = tier_scores.entry(m.tier).or_default();
                entry.0 += 1;
                if verdict == "correct" {
                    entry.1 += 1;
                }
            }

            Validation {
                checked,
                correct,
                incorrect: checked - correct,
                precision: if checked > 0 { correct as f32 / checked as f32 } else { 0.0 },
                missed,
                recall: if checked + missed > 0 {
                    correct as f32 / (checked + missed) as f32
                } else {
                    0.0
                },
                by_tier: tier_scores
                    .iter()
                    .map(|(tier, &(checked, correct))| TierAccuracy {
                        tier: tier.as_str().to_string(),
                        checked,
                        correct,
                        incorrect: checked - correct,
                        precision: if checked > 0 { correct as f32 / checked as f32 } else { 0.0 },
                    })
                    .collect(),
            }
        });

        Report {
            source: source.name.clone(),
            target: target.name.clone(),
            summary: Summary {
                source_functions: source.graph.len(),
                target_functions: target.graph.len(),
                matched: result.matches.len(),
                renames,
                confident_renames: rename_tiers.get(&MatchTier::Confident).copied().unwrap_or(0),
                candidate_renames: rename_tiers
                    .iter()
                    .filter(|(tier, _)| **tier != MatchTier::Confident)
                    .map(|(_, count)| count)
                    .sum(),
                by_tier: tier_counts
                    .iter()
                    .map(|(tier, count)| (tier.as_str().to_string(), *count))
                    .collect(),
                by_method: method_counts
                    .iter()
                    .map(|(method, count)| (method.to_string(), *count))
                    .collect(),
                rounds: result.rounds,
                validation,
            },
            matches,
            unmatched_source,
            unmatched_target,
        }
    }

    /// Matches that would give a currently-unnamed target function a name.
    fn renameable(&self) -> impl Iterator<Item = &ReportMatch> {
        self.matches.iter().filter(|m| m.renames)
    }

    fn print_summary(&self) {
        let s = &self.summary;
        info!("Matched {}/{} target functions", s.matched, s.target_functions);
        for (method, count) in &s.by_method {
            info!("  {:>12}: {}", method, count);
        }
        for (tier, count) in &s.by_tier {
            info!("  {:>12}: {}", tier, count);
        }
        info!(
            "{} target functions would gain a name: {} confident, {} needing review",
            s.renames, s.confident_renames, s.candidate_renames
        );
        if let Some(v) = &s.validation {
            info!(
                "Validation: {}/{} correct ({:.2}% precision), {} missed ({:.2}% recall)",
                v.correct,
                v.checked,
                v.precision * 100.0,
                v.missed,
                v.recall * 100.0
            );
            for t in &v.by_tier {
                info!(
                    "  {:>12}: {}/{} correct ({:.2}%), {} wrong",
                    t.tier,
                    t.correct,
                    t.checked,
                    t.precision * 100.0,
                    t.incorrect
                );
            }
        }
    }
}

fn tier_name<S>(tier: &MatchTier, serializer: S) -> Result<S::Ok, S::Error>
where S: serde::Serializer {
    serializer.serialize_str(tier.as_str())
}

fn unmatched(target: &MatchTarget, map: &[Option<u32>]) -> Vec<String> {
    map.iter()
        .enumerate()
        .filter(|(_, matched)| matched.is_none())
        .map(|(node, _)| target.symbol_name(node as u32).to_string())
        .collect()
}

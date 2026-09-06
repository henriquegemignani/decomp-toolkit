use std::collections::{HashMap, HashSet};

use crate::{
    analysis::{
        data_matching::DataMatch,
        matching::{MatchResult, MatchTarget, MatchTier},
    },
    obj::{ObjSectionKind, ObjSymbolKind, SectionIndex},
};

/// How much trust a proposed split boundary has earned.
///
/// Mirrors [`MatchTier`]: only [`Confident`](UnitTier::Confident) is safe to
/// write into a splits file unreviewed — a wrong boundary silently pulls the
/// wrong code into a unit with nothing to prompt a re-check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UnitTier {
    /// Full member count, matched confident, in source order, edges bounded,
    /// no overlap with an existing split, and (for a code run) no non-code
    /// source content left behind uncovered.
    Confident,
    /// Same shape of evidence with something unproven — see [`classify`]'s
    /// reasons.
    Candidate,
}

/// A proposed split boundary for one unit, derived from function and data
/// symbol matches.
#[derive(Debug, Clone)]
pub struct UnitProposal {
    pub unit: String,
    pub section: SectionIndex,
    pub start: u32,
    pub end: u32,
    /// Target functions or data symbols covered by this run, in address order.
    pub members: Vec<u32>,
    pub tier: UnitTier,
    /// Empty when [`UnitTier::Confident`]; otherwise what kept it from being.
    pub reasons: Vec<&'static str>,
}

/// One item in a target section's address layout — a function node when
/// grouping code, a data symbol when grouping data. [`group_runs`] doesn't
/// need to know which; it just needs contiguous, addressable, attributable
/// pieces to group.
struct Item {
    index: u32,
    section: SectionIndex,
    start: u32,
    end: u32,
}

/// Groups the target's matched functions and data symbols into contiguous
/// same-section, same-unit runs and proposes a split boundary for each.
///
/// Data runs are proposed first, since whether a code run counts as
/// [`UnitTier::Confident`] depends on whether its source unit's non-code
/// content (if any) was itself confidently covered by a data run — see
/// [`non_text_migrated_units`].
pub fn propose_units(
    source: &MatchTarget,
    target: &MatchTarget,
    result: &MatchResult,
    data_matches: &[DataMatch],
) -> Vec<UnitProposal> {
    let data_attribution = attribute_data(source, data_matches);
    let data_layout = data_layout(target);
    let source_data_counts = source_unit_counts(source, false);
    let data_proposals =
        group_runs(target, &data_layout, &data_attribution, &source_data_counts, |_| true);

    let text_only = text_only_units(source);
    let migrated = non_text_migrated_units(source, target, &data_proposals);

    let function_attribution = attribute_functions(source, result);
    let function_layout: Vec<Item> = target
        .layout()
        .iter()
        .map(|&n| {
            let node = target.graph.node(n);
            Item {
                index: n,
                section: node.section,
                start: node.address,
                end: node.address + node.size,
            }
        })
        .collect();
    let source_function_counts = source_unit_counts(source, true);
    let code_proposals = group_runs(
        target,
        &function_layout,
        &function_attribution,
        &source_function_counts,
        |unit| text_only.contains(unit) || migrated.contains(unit),
    );

    code_proposals.into_iter().chain(data_proposals).collect()
}

/// Attributes each target function to a source unit and remembers the tier
/// that vouches for it, plus the source function's own address (to check
/// source-order below). Probable matches are trusted for grouping — their
/// disagreement tends to be between siblings in the same unit — but a run
/// still needs every member at Confident to be Confident itself.
fn attribute_functions(
    source: &MatchTarget,
    result: &MatchResult,
) -> HashMap<u32, (String, MatchTier, u32)> {
    let mut attribution = HashMap::new();
    for m in &result.matches {
        let tier = m.tier();
        if !matches!(tier, MatchTier::Confident | MatchTier::Probable) {
            continue;
        }
        if let Some(unit) = source.unit_of(m.source) {
            let source_address = source.graph.node(m.source).address;
            attribution.insert(m.target, (unit.to_string(), tier, source_address));
        }
    }
    attribution
}

/// Attributes each target data symbol to a source unit via [`DataMatch`].
/// Every surviving `DataMatch` already cleared a strict bar (alignment inside
/// a confidently-matched function, with no disagreement anywhere), so unlike
/// function attribution there's no weaker tier to carry through.
fn attribute_data(
    source: &MatchTarget,
    data_matches: &[DataMatch],
) -> HashMap<u32, (String, MatchTier, u32)> {
    let mut attribution = HashMap::new();
    for dm in data_matches {
        if let Some(unit) = source.unit_of_symbol(dm.source) {
            let source_address = source.obj.symbols[dm.source].address as u32;
            attribution.insert(dm.target, (unit.to_string(), MatchTier::Confident, source_address));
        }
    }
    attribution
}

/// Every sized data (`ObjSymbolKind::Object`) symbol in the target's non-code
/// sections, sorted by section then address — the data equivalent of
/// [`MatchTarget::layout`], which only covers functions.
fn data_layout(target: &MatchTarget) -> Vec<Item> {
    let mut items: Vec<Item> = target
        .obj
        .symbols
        .iter()
        .filter(|(_, s)| s.kind == ObjSymbolKind::Object && s.size_known && s.size > 0)
        .filter_map(|(index, s)| {
            let section = s.section?;
            (target.obj.sections.get(section)?.kind != ObjSectionKind::Code).then_some(Item {
                index,
                section,
                start: s.address as u32,
                end: s.address as u32 + s.size as u32,
            })
        })
        .collect();
    items.sort_by_key(|it| (it.section, it.start));
    items
}

/// How many source functions (or, with `functions: false`, data symbols) each
/// unit claims, so a run that found all of them can be told apart from one
/// that only found some.
fn source_unit_counts(source: &MatchTarget, functions: bool) -> HashMap<&str, usize> {
    let mut counts = HashMap::new();
    if functions {
        for (node, _) in source.graph.iter() {
            if let Some(unit) = source.unit_of(node) {
                *counts.entry(unit).or_default() += 1;
            }
        }
    } else {
        for (_, s) in source.obj.symbols.iter() {
            if s.kind != ObjSymbolKind::Object || !s.size_known || s.size == 0 {
                continue;
            }
            let Some(section) = s.section.and_then(|i| source.obj.sections.get(i)) else {
                continue;
            };
            if section.kind == ObjSectionKind::Code {
                continue;
            }
            if let Some((_, split)) = section.splits.for_address(s.address as u32) {
                *counts.entry(split.unit.as_str()).or_default() += 1;
            }
        }
    }
    counts
}

/// Groups `layout` into contiguous same-section, same-unit runs (per
/// `attribution`) and classifies each as a [`UnitProposal`].
///
/// `non_text_migrated` answers, for a run's unit, whether any non-code source
/// content it owns has already been accounted for elsewhere — trivially true
/// for a data run, since it has no further content to leave behind.
fn group_runs(
    target: &MatchTarget,
    layout: &[Item],
    attribution: &HashMap<u32, (String, MatchTier, u32)>,
    source_unit_size: &HashMap<&str, usize>,
    non_text_migrated: impl Fn(&str) -> bool,
) -> Vec<UnitProposal> {
    let mut proposals = Vec::new();
    let mut i = 0;
    while i < layout.len() {
        let Some((unit, _, _)) = attribution.get(&layout[i].index) else {
            i += 1;
            continue;
        };
        let section = layout[i].section;
        let same_run = |j: usize| -> bool {
            layout.get(j).is_some_and(|it| {
                it.section == section
                    && attribution.get(&it.index).is_some_and(|(u, _, _)| u == unit)
            })
        };
        let mut j = i + 1;
        while same_run(j) {
            j += 1;
        }

        let run = &layout[i..j];
        let unit = unit.clone();
        let start = run[0].start;
        let end = run.last().unwrap().end;

        // Already split correctly: nothing to propose.
        if existing_split(target, section, start) == Some((start, end, unit.as_str())) {
            i = j;
            continue;
        }

        let same_section =
            |k: usize| -> bool { layout.get(k).is_some_and(|it| it.section == section) };
        let left_pinned = i == 0
            || !same_section(i - 1)
            || attribution.contains_key(&layout[i - 1].index)
            || has_split_at(target, section, start);
        let right_pinned = !same_section(j)
            || attribution.contains_key(&layout[j].index)
            || has_split_at(target, section, end);

        // Sections don't overlap in address space, so comparing the matched
        // source addresses directly orders members the same way sorting by
        // (section, address) would.
        let source_positions: Vec<u32> = run
            .iter()
            .filter_map(|it| attribution.get(&it.index))
            .map(|&(_, _, addr)| addr)
            .collect();
        let monotonic = source_positions.windows(2).all(|w| w[0] < w[1]);

        let all_confident = run.iter().all(|it| {
            attribution.get(&it.index).is_some_and(|(_, t, _)| *t == MatchTier::Confident)
        });

        let expected = source_unit_size.get(unit.as_str()).copied().unwrap_or(0);
        let (tier, reasons) = classify(
            run.len(),
            expected,
            left_pinned,
            right_pinned,
            monotonic,
            overlaps_split(target, section, start, end),
            all_confident,
            non_text_migrated(unit.as_str()),
        );

        proposals.push(UnitProposal {
            unit,
            section,
            start,
            end,
            members: run.iter().map(|it| it.index).collect(),
            tier,
            reasons,
        });
        i = j;
    }
    proposals
}

/// Source units whose every declared split lives in a code section.
///
/// Migrating just the `.text` of a unit that also owns data/`.ctors`/extab
/// content leaves that content claimed by a leftover auto-generated object —
/// multiply-defined or undefined at link time — unless the data itself was
/// also migrated, which [`non_text_migrated_units`] checks for separately.
fn text_only_units(source: &MatchTarget) -> HashSet<String> {
    let mut all_units: HashSet<&str> = HashSet::new();
    let mut has_non_code: HashSet<&str> = HashSet::new();
    for (_, section) in source.obj.sections.iter() {
        for (_, split) in section.splits.iter() {
            all_units.insert(split.unit.as_str());
            if section.kind != ObjSectionKind::Code {
                has_non_code.insert(split.unit.as_str());
            }
        }
    }
    all_units.difference(&has_non_code).map(|&s| s.to_string()).collect()
}

/// Source units whose non-code sections are *all* covered by a Confident data
/// proposal in the target — i.e. migrating this unit's `.text` won't leave
/// any of its data behind unaccounted for.
fn non_text_migrated_units(
    source: &MatchTarget,
    target: &MatchTarget,
    data_proposals: &[UnitProposal],
) -> HashSet<String> {
    let mut needed: HashMap<&str, HashSet<&str>> = HashMap::new();
    for (_, section) in source.obj.sections.iter() {
        if section.kind == ObjSectionKind::Code {
            continue;
        }
        for (_, split) in section.splits.iter() {
            needed.entry(split.unit.as_str()).or_default().insert(section.name.as_str());
        }
    }

    let mut covered: HashMap<&str, HashSet<&str>> = HashMap::new();
    for p in data_proposals {
        if p.tier != UnitTier::Confident {
            continue;
        }
        if let Some(name) = target.obj.sections.get(p.section).map(|s| s.name.as_str()) {
            covered.entry(p.unit.as_str()).or_default().insert(name);
        }
    }

    needed
        .into_iter()
        .filter(|(unit, sections)| covered.get(unit).is_some_and(|got| sections.is_subset(got)))
        .map(|(unit, _)| unit.to_string())
        .collect()
}

/// Classifies a run of matched members from the evidence already gathered
/// about it.
///
/// Kept separate from the address/lookup mechanics in [`group_runs`] so the
/// classification rules can be tested without a real binary.
#[allow(clippy::too_many_arguments)]
fn classify(
    run_len: usize,
    expected: usize,
    left_pinned: bool,
    right_pinned: bool,
    source_order_monotonic: bool,
    overlaps_existing: bool,
    all_confident: bool,
    non_text_content_migrated: bool,
) -> (UnitTier, Vec<&'static str>) {
    let mut reasons = Vec::new();
    if run_len < expected {
        reasons.push("target is missing functions the source unit has");
    }
    if !left_pinned {
        reasons.push("left edge borders an unmatched function");
    }
    if !right_pinned {
        reasons.push("right edge borders an unmatched function");
    }
    if !source_order_monotonic {
        reasons.push("source functions are out of order");
    }
    if overlaps_existing {
        reasons.push("overlaps an existing split");
    }
    if !all_confident {
        reasons.push("contains a function matched below the confident tier");
    }
    if !non_text_content_migrated {
        reasons.push("source unit has non-text content that wasn't migrated");
    }
    let tier = if reasons.is_empty() { UnitTier::Confident } else { UnitTier::Candidate };
    (tier, reasons)
}

fn has_split_at(target: &MatchTarget, section: SectionIndex, address: u32) -> bool {
    target.obj.sections.get(section).is_some_and(|s| s.splits.has_split_at(address))
}

fn overlaps_split(target: &MatchTarget, section: SectionIndex, start: u32, end: u32) -> bool {
    let Some(s) = target.obj.sections.get(section) else { return false };
    s.splits.for_range(start..end).next().is_some() || s.splits.for_address(start).is_some()
}

/// The existing split starting exactly at `address`, if any, as
/// `(start, end, unit)`.
fn existing_split(
    target: &MatchTarget,
    section: SectionIndex,
    address: u32,
) -> Option<(u32, u32, &str)> {
    let s = target.obj.sections.get(section)?;
    let (addr, split) = s.splits.for_address(address)?;
    (addr == address).then_some((addr, split.end, split.unit.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline(run_len: usize, expected: usize) -> (UnitTier, Vec<&'static str>) {
        classify(run_len, expected, true, true, true, false, true, true)
    }

    #[test]
    fn a_complete_pinned_run_is_confident() {
        let (tier, reasons) = baseline(3, 3);
        assert_eq!(tier, UnitTier::Confident);
        assert!(reasons.is_empty());
    }

    #[test]
    fn a_partial_run_is_a_candidate() {
        // The source unit has more functions than the target run found.
        let (tier, reasons) = baseline(2, 3);
        assert_eq!(tier, UnitTier::Candidate);
        assert_eq!(reasons, vec!["target is missing functions the source unit has"]);
    }

    #[test]
    fn an_unpinned_edge_is_a_candidate() {
        let (tier, reasons) = classify(3, 3, false, true, true, false, true, true);
        assert_eq!(tier, UnitTier::Candidate);
        assert_eq!(reasons, vec!["left edge borders an unmatched function"]);

        let (tier, reasons) = classify(3, 3, true, false, true, false, true, true);
        assert_eq!(tier, UnitTier::Candidate);
        assert_eq!(reasons, vec!["right edge borders an unmatched function"]);
    }

    #[test]
    fn a_reordered_run_is_a_candidate() {
        let (tier, reasons) = classify(3, 3, true, true, false, false, true, true);
        assert_eq!(tier, UnitTier::Candidate);
        assert_eq!(reasons, vec!["source functions are out of order"]);
    }

    #[test]
    fn overlap_with_an_existing_split_is_a_candidate() {
        let (tier, reasons) = classify(3, 3, true, true, true, true, true, true);
        assert_eq!(tier, UnitTier::Candidate);
        assert_eq!(reasons, vec!["overlaps an existing split"]);
    }

    #[test]
    fn a_non_confident_member_is_a_candidate() {
        // Attribution trusts Probable matches for grouping, but a run isn't
        // safe to write unreviewed unless every member earned Confident.
        let (tier, reasons) = classify(3, 3, true, true, true, false, false, true);
        assert_eq!(tier, UnitTier::Candidate);
        assert_eq!(reasons, vec!["contains a function matched below the confident tier"]);
    }

    #[test]
    fn a_source_unit_with_unmigrated_data_is_a_candidate() {
        // Migrating .text alone would leave the unit's data claimed by
        // whatever leftover object still covers it under another name.
        let (tier, reasons) = classify(3, 3, true, true, true, false, true, false);
        assert_eq!(tier, UnitTier::Candidate);
        assert_eq!(reasons, vec!["source unit has non-text content that wasn't migrated"]);
    }

    #[test]
    fn multiple_problems_all_get_reported() {
        let (tier, reasons) = classify(2, 3, false, false, true, false, true, true);
        assert_eq!(tier, UnitTier::Candidate);
        assert_eq!(reasons, vec![
            "target is missing functions the source unit has",
            "left edge borders an unmatched function",
            "right edge borders an unmatched function",
        ]);
    }
}

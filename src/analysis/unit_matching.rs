use std::collections::{HashMap, HashSet};

use crate::{
    analysis::{
        callgraph::NodeIndex,
        matching::{MatchResult, MatchTarget, MatchTier},
    },
    obj::{ObjSectionKind, SectionIndex},
};

/// How much trust a proposed split boundary has earned.
///
/// Mirrors [`MatchTier`]: only [`Confident`](UnitTier::Confident) is safe to
/// write into a splits file unreviewed — a wrong boundary silently pulls the
/// wrong code into a unit with nothing to prompt a re-check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UnitTier {
    /// Full function count, matched confident, in source order, edges
    /// bounded, no overlap with an existing split, and no non-`.text`
    /// source content left behind.
    Confident,
    /// Same shape of evidence with something unproven — see [`classify`]'s
    /// reasons.
    Candidate,
}

/// A proposed split boundary for one unit, derived from function matches.
///
/// Only `.text`-shaped ranges are proposed: a function match says nothing
/// about which unit a data symbol belongs to. A source unit with any
/// non-code content is downgraded to [`UnitTier::Candidate`] instead of
/// silently dropped — see [`text_only_units`].
#[derive(Debug, Clone)]
pub struct UnitProposal {
    pub unit: String,
    pub section: SectionIndex,
    pub start: u32,
    pub end: u32,
    /// Target functions covered by this run, in address order.
    pub functions: Vec<NodeIndex>,
    pub tier: UnitTier,
    /// Empty when [`UnitTier::Confident`]; otherwise what kept it from being.
    pub reasons: Vec<&'static str>,
}

/// Groups the target's matched functions into contiguous same-section,
/// same-unit runs and proposes a split boundary for each.
pub fn propose_units(
    source: &MatchTarget,
    target: &MatchTarget,
    result: &MatchResult,
) -> Vec<UnitProposal> {
    // Attribute each target function to a source unit and remember the tier
    // that vouches for it. Probable matches are trusted for grouping —
    // their disagreement tends to be between siblings in the same unit —
    // but a run still needs every member at Confident to be Confident itself.
    let mut attribution: HashMap<NodeIndex, (String, MatchTier)> = HashMap::new();
    for m in &result.matches {
        let tier = m.tier();
        if !matches!(tier, MatchTier::Confident | MatchTier::Probable) {
            continue;
        }
        if let Some(unit) = source.unit_of(m.source) {
            attribution.insert(m.target, (unit.to_string(), tier));
        }
    }

    // How many source functions each unit claims, so a run that found all of
    // them can be told apart from one that only found some.
    let mut source_unit_size: HashMap<&str, usize> = HashMap::new();
    for (node, _) in source.graph.iter() {
        if let Some(unit) = source.unit_of(node) {
            *source_unit_size.entry(unit).or_default() += 1;
        }
    }

    let text_only = text_only_units(source);

    let layout = target.layout();
    let mut proposals = Vec::new();
    let mut i = 0;
    while i < layout.len() {
        let Some((unit, _)) = attribution.get(&layout[i]) else {
            i += 1;
            continue;
        };
        let section = target.graph.node(layout[i]).section;
        let same_run = |j: usize| -> bool {
            layout.get(j).is_some_and(|&n| {
                target.graph.node(n).section == section
                    && attribution.get(&n).is_some_and(|(u, _)| u == unit)
            })
        };
        let mut j = i + 1;
        while same_run(j) {
            j += 1;
        }

        let run = &layout[i..j];
        let unit = unit.clone();
        let first = target.graph.node(run[0]);
        let last = target.graph.node(*run.last().unwrap());
        let start = first.address;
        let end = last.address + last.size;

        // Already split correctly: nothing to propose.
        if existing_split(target, section, start) == Some((start, end, unit.as_str())) {
            i = j;
            continue;
        }

        let same_section = |k: usize| -> bool {
            layout.get(k).is_some_and(|&n| target.graph.node(n).section == section)
        };
        let left_pinned = i == 0
            || !same_section(i - 1)
            || attribution.contains_key(&layout[i - 1])
            || has_split_at(target, section, start);
        let right_pinned = !same_section(j)
            || attribution.contains_key(&layout[j])
            || has_split_at(target, section, end);

        let source_positions: Vec<u32> = run
            .iter()
            .filter_map(|&n| result.target_to_source[n as usize])
            .map(|s| source.layout_position(s))
            .collect();
        let monotonic = source_positions.windows(2).all(|w| w[0] < w[1]);

        let all_confident =
            run.iter().all(|n| attribution.get(n).is_some_and(|(_, t)| *t == MatchTier::Confident));

        let expected = source_unit_size.get(unit.as_str()).copied().unwrap_or(0);
        let (tier, reasons) = classify(
            run.len(),
            expected,
            left_pinned,
            right_pinned,
            monotonic,
            overlaps_split(target, section, start, end),
            all_confident,
            text_only.contains(unit.as_str()),
        );

        proposals.push(UnitProposal {
            unit,
            section,
            start,
            end,
            functions: run.to_vec(),
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
/// multiply-defined or undefined at link time. Measured on a real migration:
/// 169 of 237 confidently-matched units hit exactly this before this check
/// existed.
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

/// Classifies a run of matched functions from the evidence already gathered
/// about it.
///
/// Kept separate from the address/lookup mechanics in [`propose_units`] so the
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
    source_is_text_only: bool,
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
    if !source_is_text_only {
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
    fn a_source_unit_with_data_is_a_candidate() {
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

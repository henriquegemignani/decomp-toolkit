# Symbol matching: planned work

## Audit update — 2026-09-09

The measurements below are historical, not current guarantees. The external
workflow now lives in the `dtk-version-matching` repository, alongside the Prime
checkout, not in Prime's `tools/` directory. Its README and
`docs/validation_audit.md` describe the corrected process.

The earlier compile-verify loop had a circular validation path: it left new units
disabled in `configure.py`, then linked their extracted original objects and
compared those bytes with retail. Consequently its successful retail hash and
"direct ELF byte comparison" did **not** verify candidate compiled source.
`metadata.complete` is a build-configuration flag; per-section fuzzy scores are
comparison results and can omit extra compiler output or ignore relocation
differences. Neither is a whole-file byte-equality proof.

The corrected workflow separates `discover_splits.py` (retain code splits that
increase compiler-matched bytes while preserving existing progress and retail
split integrity) from `verify_source_units.py` (enable compiled objects, check
their actual linker dependencies, then require both the retail checksum and raw
DOL equality). Partial code matches are useful without a fully matching file.

The first 100-proposal PAL discovery pass increased matched code from
256,284 / 3,908,836 bytes (6.5565%) to 648,568 / 3,908,836 (16.5924%), without manual
target boundary or source-code edits. This is objdiff progress, not 16.59% verified
source linkage. See the external audit for final source-link results.

Read-only `_00` → `_02` hidden-name calibration with DTK executable SHA-256
`c627ef0838672a2f10f328c88069a09d50765ec232351d9dd158345160c61b57` checked 1,339 named
matches: 1,334 agreed and 5 disagreed (99.6266%). Confident-tier agreement was
1,240 / 1,243 (99.7586%). One disagreement is a compiler-generated array-dtor
suffix; two others pair unrelated-looking names and remain unadjudicated.
Do not repeat the historical claim that every confident disagreement is known
ground-truth noise. `_02` is a useful mixed NTSC/PAL calibration target; PAL is
still the primary migration target.

Current priorities: improve data/relocation attribution and whole-file boundary
coverage, record evidence provenance, and distinguish a failed proposal from a
permanently unmigratable unit. The existing uncommitted `bridge_gap` change was
left untouched. The existing executable was used without rebuilding; its hash
above is the precise tool identifier for these measurements.

Notes for extending the [`match`](../README.md#match) command. Written after the initial
implementation and a round of measurement against the Metroid Prime decompilation, which has seven
versions of the same executable and is the driving use case.

## Governing principle

**An absent symbol name is better than a possibly-wrong one.**

A wrong name is worse than no name because it silently misleads. It looks like established
knowledge, propagates into decompiled source and onward into further version ports, and nothing
prompts anyone to re-check it. A missing name is visibly missing and costs only a lookup.

Everything below follows from this. The tool may *suggest* freely, but it must never *assert*
something it isn't sure of, and the two must never share an output channel.

## Where things stand

Accuracy against versions that already have names, with names hidden during matching (`--validate`):

| Target | Matched | Precision | Recall | Scored on |
|---|---|---|---|---|
| `GM8E01_01` | 16603/16604 | 98.64% | 98.63% | 15,093 |
| `GM8E01_02` | 16589/16606 | 99.85% | 99.70% | 1,331 |
| `GM8E01_48` | 16600/16601 | 99.30% | 99.27% | 3,562 |
| `GM8J01_00` (JP) | 12450/14707 | 99.15% | 97.87% | 1,064 |
| `GM8P01_00` (PAL) | 12446/14709 | 99.88% | 99.76% | 842 |
| `R3ME01_00` (Trilogy) | 5340/16001 | 97.26% | 73.58% | 146 |

The `GM8E01_01` figure understates reality: its 206 "errors" all sit at the address delta shared by
the bulk of correct matches, and are truncated names or human naming drift (`CARDStat`/`CardStat`,
`CShockWave`/`CShockwave`) rather than mismatches.

`R3ME01_00` is the Wii Trilogy port and the closest available proxy for the eventual cross-game
target. Content hashing nearly collapses there (299 exact-hash anchors, against 5,540 for JP) and
link order stops transferring (160 layout matches, against 3,315). Call-graph propagation carries
it: 336 tier-1 anchors bootstrap ~5,000 matches, a 15× amplification.

## Task 1: separate confident output from suggestions — **done**

Implemented. `Match` now carries whether the two bodies hash identically and which source function
came closest to winning, and `Match::tier()` classifies on that evidence rather than on a confidence
cut. `--renames` carries confident matches only; `--candidates` carries the rest with their
alternatives.

An identical body only counts as evidence above `MIN_DISTINCTIVE_INSTRUCTIONS`. Below it, bodies
collide by coincidence — a bare `blr` is byte-identical to every other stub in the binary, and a
body that couldn't be read at all hashes like every other empty one. The hash *anchor* already
required this; the promotion did not, which let stub-sized pairs reach `confident` on a single
layout vote. Both now share the constant.

Measured with `--validate`, which hides the target's names and scores against them afterwards:

| Target | confident | probable | candidate |
|---|---|---|---|
| `GM8E01_01` | 10818/11013 (98.23%) | 381/384 (99.22%) | 3688/3696 (99.78%) |
| `GM8E01_02` | 1236/1237 (99.92%) | 77/77 (100%) | 16/17 (94.12%) |
| `GM8E01_48` | 3317/3339 (99.34%) | 151/153 (98.69%) | 69/70 (98.57%) |
| `GM8P01_00` | **791/791 (100%)** | 43/43 (100%) | 7/8 (87.50%) |
| `R3ME01_00` | **43/43 (100%)** | **33/33 (100%)** | 66/70 (94.29%) |

At that historical checkpoint, the inspected confident disagreements were attributed
to ground-truth noise: truncated names, `CARDStat`/`CardStat` capitalisation
drift, and `__arraydtor$381`/`__arraydtor$159` compiler-generated suffixes. This is not
a guarantee for current inputs; see the audit above. On the hardest pair all
four real errors sit in `candidate`, and both trusted tiers are clean.

The guard was not theoretical. It moved ~3,900 matches per close-revision pair out of `confident`
(`GM8E01_02` confident renames fell from 11,398 to 7,526), and it removed the one real confident
error found before it existed — `__close_console` → `__write_console`, two console stubs with
identical short bodies matched on layout position. Measurements taken before the guard overstated
what `confident` had earned.

The design it was built to, kept for reference:

- **Confident** — safe to apply unreviewed. Earned by identical relocation-masked encoding, a
  unique string anchor, or agreement between independent sources (task 2). Written to `--renames`.
- **Probable** — a single well-corroborated match with no close runner-up. Written to a separate
  candidates file, never to `--renames`.
- **Candidate** — matched, but weakly. Written to the candidates file *with its runner-up
  alternatives*, so a reviewer sees what else it could be.
- **Conflicted** — evidence disagrees. Emit every option and name nothing.

Three tiers shipped, not four. **Conflicted** was folded into **Candidate**, because with a single
source there's nothing for evidence to disagree *between*: an exactly-tied vote is already deferred
during propagation rather than committed, and a near-tie demotes to Candidate with the rival shown.
The tier becomes meaningful once task 2 lands and two sources can propose different names for the
same function.

Do not write candidates into `symbols.txt` as comments. It invites accidental promotion to real
names and risks confusing the existing parser.

Tiering must not be a single confidence threshold. The measurements show confidence behaves
differently depending on how similar the two binaries are:

- Close revisions produce a *bimodal* distribution — on `_00` → `_02`, layout matches land at
  either 0.867 (identical opcodes and reference counts, different operands) or 0.98 (byte-identical
  after masking), with 10 of 7,061 anywhere else. A threshold barely discriminates.
- Distant binaries produce a *broad* distribution — on `_00` → `R3ME01`, call-site confidences
  spread across 0.56–0.98, and all four confirmed errors sit at 0.74–0.76. Here a threshold works
  well.

So tier on the underlying evidence (encoding equality, anchor kind, source agreement, margin over
runner-up), and treat the scalar confidence as one input rather than the decision.

## Task 2: multi-source consensus

Accept several source configurations and combine their results:

```shell
$ dtk match --source config/GM8E01_00/config.yml \
            --source config/GM8P01_00/config.yml \
            config/R3ME01_00/config.yml
```

Two distinct benefits, measured against `R3ME01_00`.

**Coverage — modest, and only from independent versions.** Target functions reached:

| Source | Alone | Adds to `_00` |
|---|---|---|
| `_00` | 5,449 | — |
| `_48` | 5,447 | **0** |
| PAL | 5,544 | +416 |
| JP | 5,527 | +402 |
| union of all four | 5,867 | +5.8% over the best single source |

`_48` adds *nothing*: it is 99.99% identical to `_00`, so it finds the same anchors. Chaining
`_00 → _48 → R3ME01` likewise added zero coverage over matching directly. JP adds only 2 beyond
PAL. The seven versions therefore contain **two** independent axes — {NTSC ×4} and {JP, PAL} — and
everything else is redundant. 87% of the union is reached by all four sources, so the ceiling is
the target's divergence, not source coverage.

**Precision — the real payoff.** Two independent routes to the same target:

```
A: _00 → R3ME01                 5,449 matches
B: _00 → PAL → R3ME01           5,395 matches
both reach                      5,103
  agree                         5,082  (99.59%)
  disagree                         21
```

The disagreements are exactly the failure mode worth catching — adjacent members of the same class,
where neither route is obviously right:

```
CPlasmaProjectile:    RenderMotionBlur    vs  UpdateEnergyPulse
CHudHelmetInterface:  UpdateHelmetAlpha   vs  Update
CGameState::PutTo     vs  CSystemState::PutTo
rstl::vector::clear   vs  rstl::vector::reserve
```

This matters most where it's needed most. On `R3ME01_00` only 146 functions could be scored against
ground truth; cross-route agreement independently corroborates 5,082. For a genuinely new target
with no known symbols, it is the *only* validation available.

Under the governing principle, agreement promotes a match to **confident**, and disagreement
demotes it to **conflicted** — emit every candidate, name nothing.

Two caveats to encode:

- Sources must come from different axes. Combining `_00` and `_48` produces false reassurance:
  identical inputs agree trivially. The tool should warn when two sources match each other above
  some very high threshold, since it means their agreement carries no information.
- Agreement is not fully independent when routes share a prefix. Both routes above pass through
  `_00`'s names, so an error in `_00 → PAL` propagates into route B. Genuine independence needs
  disjoint paths.

## Implementation sketch

Roughly in dependency order:

1. ~~**Provenance on matches.**~~ Done: `Match` carries `distinctive_body` and `runner_up`.
2. ~~**Tier classification.**~~ Done: `Match::tier()`, driven by evidence kind.
3. ~~**Split the output.**~~ Done: `--renames` is confident-only, `--candidates` takes the rest.
4. **Repeatable `--source`.** Run the matcher once per source against the shared target. The loads
   are independent, so this parallelizes, but note that `WorkingDirectory` in
   [`src/cmd/symbol_match.rs`](../src/cmd/symbol_match.rs) mutates process-global state and must be
   reworked before any load runs concurrently.
5. **Consensus merge.** A new module joining N `MatchResult`s on target node: agreement count,
   disagreement detection, tier promotion and demotion.
6. **Source-independence warning.** Cheap check that two sources aren't near-clones.

## Task 3: splits migration — first real run and what it taught

`dtk match --splits` shipped (`src/analysis/unit_matching.rs`) and was run for real on 2026-09-06,
NTSC `GM8E01_00` → PAL `GM8P01_00`, in `C:\Users\henri\programming\decomp\prime`. Outcome:

| Stage | Units |
|---|---|
| Confident split proposals | 237 |
| Reverted: source unit has non-`.text` content we didn't migrate | −169 |
| Reverted: depends on unmatched code (undefined symbols at link) | −9 |
| **Linked for PAL, `ninja` clean** | **58** |
| Of which byte-identical per `report.json` | 260 functions, SDK 98%, Kyoto 81% |

Also applied: 6,047 confident function renames (3 skipped on genuine collisions), and `scope:local`
on 11 duplicate template-instantiation names. Everything in the prime repo is **uncommitted** as of
writing (`splits.txt`, `symbols.txt`, `configure.py` for GM8P01_00). Full detail in
[match_learnings.md](match_learnings.md#findings-from-the-first-real-migration).

The tasks below are ordered by what recovers the most units per unit of effort, and each is
independent of the multi-source work above.

### Task 3a: tighten the Confident tier (stop the bleeding) — **done**

Implemented and validated against the same PAL migration on 2026-09-07.

- ~~Require **every** member function of a run to be a Confident match~~. Done: `propose_units`
  still trusts Probable for attribution (grouping), but `classify()` now takes `all_confident` and
  adds `"contains a function matched below the confident tier"` when any member falls short.
- ~~Require the **source unit to be `.text`-only**~~. Done: `text_only_units()` scans every section
  of the source `ObjInfo` and flags any unit with a split outside `ObjSectionKind::Code`; `classify()`
  adds `"source unit has non-text content that wasn't migrated"`. Superseded once 3c lands.
- ~~Propagate **symbol scope** through renames~~. Done, and differently than sketched: rather than
  detecting duplicate names and picking one to keep global, `MatchTarget::is_local()` copies the
  *source* symbol's own scope onto each match directly — correct per-instance, since NTSC and PAL
  don't agree with each other on which specific copy of a duplicate is the local one, so there's
  nothing to usefully detect. `--renames`/`--candidates` append a trailing `local` word;
  `Renames::parse` reads it; `apply_renames` calls a new `ensure_scope_local()` that adds
  `scope:local` to the target line's attributes unless it already declares a scope.
- **Found during validation, not in the original plan**: the collision guard in `apply_renames`
  rejected exactly the renames the `local` marker exists for — a name already held by an *existing*
  symbol is precisely the legitimate-duplicate case, not a real collision. Fixed: a `local` rename
  skips the taken-name check entirely; only a global rename still collides. Without this fix, PAL's
  three previously-collision-skipped `AlarmHandler`/`OnReset` renames stayed skipped even with the
  scope marker in hand.

Re-running against PAL's already-migrated state confirmed the fix live: the three renames applied
(all four `AlarmHandler` and four `OnReset` copies across PAL now carry `scope:local`), and
`--splits` correctly dropped from 237 to 8 new confident proposals against the same source/target,
with the 179 previously-reverted units now showing their real reasons instead of silence.

### Task 3b: verify by compiling, never by asserting — historical implementation

**Superseded by the 2026-09-09 audit above.** The historical implementation below
did not verify that candidate source objects were linked; its claimed byte-level
guarantee was therefore too strong. The external discovery and source-verification
scripts now implement separate comparison and whole-file gates.

Built as `tools/split_confidence_loop.py` in the prime repo, not in dtk (correctly — see "needs
nothing from the matcher itself" below), and matches the loop sketched here closely: stage every
proposal (confident and candidate alike, not just confident), compile-verify against
`report.json`'s per-section `fuzzy_match_percent` at 100%, real-link check, then gate on the full-DOL
hash check before anything is considered real — reverting piecemeal (dropping the specific unit a
build failure names, or the nearest staged candidate by address when it doesn't) rather than
all-or-nothing. Extended well past the original sketch: a link-order pre-check that mirrors
`resolve_link_order` in Python to catch a doomed batch before ever running a real build, direct
byte-comparison against a linked ELF for the "confident but blocked by an unclaimed neighbor" case
(see [match_learnings.md](match_learnings.md#a-confidently-matched-section-can-still-fail-comparison-because-of-its-neighbor-not-its-own-content)),
and a cheap NTSC-declaration-order neighbor-consistency check that catches a wrong section match
before ever staging it, not just after a build fails.

Result as of 2026-09-08: PAL units present grew from 247 to 282 of 814 (34.6%) across the session,
each one confirmed against the real hash check, not just a clean link — see
[match_learnings.md](match_learnings.md#findings-from-continued-pal-migration-work) for what broke
finding that number and how each was fixed.

Original plan text, for reference — still the shape of what the script above actually does:

Marking `MatchingFor(<version>)` from a function match alone is the wrong process; it asserts a
byte-match nobody checked. The loop should be:

1. Apply every proposable split with **no** `MatchingFor` change.
2. Compile-only build; read `build/<ver>/report.json` (per-unit match %).
3. Mark `MatchingFor` only for units at 100%.
4. Before linking, per candidate unit read the compiled `.o`'s undefined externals (dtk already
   parses ELF) and check each exists in the target `symbols.txt`; rename from a matched counterpart
   where possible, defer the unit otherwise. Catches `CTreeUtils::GetTransitionTree` before the
   linker does.

Turns four manual build-fix rounds into one pass and makes "converted" a measured number. Needs
nothing from the matcher itself — still the highest-value remaining task.

**Revised after the 2026-09-07 hash-checked migration below: step 3's "100%" gate is necessary but
not sufficient.** A real full-DOL hash check (not `--non-matching`, which skips it entirely) failed
on a build where linking succeeded and `report.json` showed every enabled unit as `complete: true`.
Two failure classes neither the linker nor `report.json`'s top-level `complete` flag catch:

- **A per-object `total_code`/`total_data` match doesn't cover the whole file.** A unit whose only
  confident match was its data (never its code) still gets *all* of its source compiled once
  `MatchingFor` is set for the whole path — including functions no split ever reserved space for.
  Nothing was multiply-defined (nothing else claimed that space) or undefined (nothing referenced
  it) — the linker just silently appended the extra bytes, growing the DOL and shifting every
  address after it. Step 3 needs a per-*file* check, not per-split: only mark `MatchingFor` when
  every section the source file will actually emit — text included, even if no `--splits` proposal
  covers it — is accounted for.
- **`report.json`'s `complete` flag isn't the same as byte-identical.** A section can be "complete"
  (every expected symbol accounted for) while its actual bytes differ — the per-section
  `fuzzy_match_percent` is the field that catches this, and step 3 needs to gate on *that*, at 100%,
  not on the coarser `complete` boolean. Seen for real: a string-literal data pool matched
  positionally (same function, same reference) at 95.38% — the reference was right, but PAL's actual
  string content differs from NTSC's (plausibly localization), which is legitimate game content, not
  a matcher error. Position-confidence about *which* data a function references is not the same
  claim as byte-confidence about *what's in* that data, and only the compile-and-diff step can tell
  them apart.

Both went undetected by every check available short of an actual hash-checked link — `--validate`
never touches the linker, and `--non-matching` explicitly skips the checksum gate that would have
caught them immediately. See [match_learnings.md](match_learnings.md#the-hash-check-is-the-only-real-oracle-linking-clean-is-not-enough).

### Task 3c: migrate data sections with the code — large, recovers the most units — **done**

Implemented and validated (read-only) against PAL on 2026-09-07, `src/analysis/data_matching.rs` +
`src/analysis/unit_matching.rs`.

- **Data symbol matching.** `match_data()` aligns, for every `MatchTier::Confident` function pair,
  their `data_refs()` sequences — but only positionally and all-or-nothing (same length, each
  position agreeing on relocation kind and addend), not LCS-style like call sites: there's no
  pre-existing partial map of data symbols to anchor on the way matched callees anchor a call
  sequence. A `(source, target)` pair only survives if every position it appeared in agreed, in both
  directions — the same collapse-on-disagreement rule `anchor_by_string` already used, reused via
  `merge_proposal`. Function-pointer data refs (vtable slots) are excluded; they already have their
  own match, or don't, from `match_functions`.
- **Matched data symbols go to `--renames`** too, gated the same way function renames are (source
  named, target not). Validated against the real PAL run: **2,360 new confident data renames**,
  where the previous run (function matching only) found none — real names recovered include struct
  members, static tables (`kMissileCosts`, `kComboAmmoPeriods`), vtables (`__vt__17CColorInstruction`),
  and string-pool labels, several correctly carrying `local`.
- **`propose_units` now proposes non-code runs too.** A data "layout" (every sized `Object` symbol in
  a non-code target section) is grouped the same way the function layout is, via a `group_runs()`
  the two now share. A code run's "non-text content" objection is dropped once a Confident data
  proposal covers *every* non-code section the source unit owns (`non_text_migrated_units()`) —
  `text_only_units()` from 3a still short-circuits the common case of a unit with nothing to migrate.
  Validated: the same PAL match now proposes **141 confident splits** (51 `.text`, 31 `.data`,
  30 `.sdata2`, 13 `.rodata`, 7 `.sbss`, 6 `.bss`, 3 `.sdata`) instead of function-only splits, with
  units like `Collision/CMRay.cpp` correctly getting both a `.text` and a `.sdata2` proposal together.
- **`.ctors`/`.dtors`/`extabindex`**: no new dtk code. As noted below, `split_ctors_dtors` already
  adopts a pointed-to function's unit once that function has a `.text` split — this falls out of the
  existing single-binary split analysis once 3c's function+data splits are applied and the module is
  re-analyzed, not something `dtk match` itself needs to compute.

**Applied for real on 2026-09-07**, from a freshly-reverted, freshly-pulled PAL (no prior migration
state): NTSC → PAL end to end — `dtk match`, `dtk symbols rename`, `dtk splits merge`, then
`MatchingFor("GM8P01_00")` added to every newly-split unit that was already `MatchingFor` on NTSC
(137 of 155; the other 18 aren't matching on NTSC either, so there's no basis to expect PAL to link
from source for them). First `ninja` run caught a real dtk bug — see the alignment finding in
[match_learnings.md](match_learnings.md) — fixed and re-run clean. After excluding 32 units that
still failed to link (28 multiply-defined against their own leftover object, 4 undefined-symbol
dependencies on not-yet-matching code — both the exact failure classes task 3b is meant to catch
automatically), **105 units link for PAL**, up from 58 in the first migration. `configure.py`,
`symbols.txt`, `splits.txt` are updated and uncommitted in the prime repo.

Superseded by task 3b's now-implemented compile-verify loop: 105 was a link-only, `--non-matching`
count, since revised as "necessary but not sufficient" below. As of 2026-09-08, **282 of 814 PAL
units are present**, each confirmed against the real full-DOL hash check rather than a clean link —
see task 3b above.

### Task 3d: put the apply steps into dtk — small, half done

Two throwaway Python scripts did the first migration's merging and `configure.py` editing. They
shouldn't exist.
- ~~`dtk splits merge`~~. **Done**: `src/util/split_merge.rs` + `dtk splits merge <splits.txt>
  <proposal>`, mirroring `dtk symbols rename`. Inserts confident units in address order, skips a
  name already present (reported, not overwritten), preserves the `Sections:` header verbatim and
  the file's own CRLF/LF choice. Validated on PAL: also fixed drift in the original file where two
  units had ended up out of address order from the earlier hand-rolled script's edits.
- Project side (`configure.py`'s `MatchingFor` tuples): **not done**. Depends on 3b existing first —
  there's no point automating "add a version to `MatchingFor`" while the decision of *which* units
  qualify is still manual.

### Small items

- Document that in-progress versions need `configure.py --non-matching`; the default target includes
  a full-ROM checksum that can only pass at 100%.
- PAL's five `Runtime/*` units are capitalized differently from `configure.py`'s `runtime/*` objects
  and silently fall back to original bytes ("Missing configuration for Runtime/…"). Pre-existing,
  unrelated to the migration.
- ~~`unit_matching.rs` docs say ".text-only" but it actually proposes any code section~~. **Done** as
  part of 3c's rewrite: code runs are just "code", data runs are their own thing, and the wording no
  longer implies `.text` specifically.

## Open questions

- **`.ctors`/function unit-attribution path-prefix bug.** `resolve_link_order`'s ctors/dtors
  consistency check attributed `runtime/__init_cpp_exceptions.cpp`'s `.ctors` entry to a unit missing
  its directory prefix while the function kept it, treating one real unit as two. Not yet
  root-caused to a specific line or fixed; see
  [match_learnings.md](match_learnings.md#the-ctorsfunction-unit-attribution-check-has-a-path-prefix-bug-open).
- **BSS's residual link-order fragility.** Even after distinguishing common from regular BSS
  (task 3b work, 2026-09-08), a smaller cyclic component can still occur among regular BSS with no
  individually-wrong candidate found — current best explanation is alignment/size-driven packing
  that isn't strictly monotonic with declaration order, just usually close enough. Treating BSS edges
  as soft/tie-breaking rather than a hard toposort constraint is the likely direction; not attempted.
  See [match_learnings.md](match_learnings.md#bss-address-adjacency-is-not-a-reliable-link-order-signal-but-the-reason-is-narrower-than-no-file-content).
- **Exposing `use_layout`.** `MatchOptions::use_layout` has no CLI flag. Link-order inference
  assumes a shared translation-unit layout, which holds across revisions of one game and not across
  different games. It needs a flag before the cross-game work, and the default should probably
  invert when sources look unrelated.
- **Layout's floor.** An uncontested single layout vote scores ~0.667 before body similarity is
  considered, which clears the default 0.5 threshold on position alone. Harmless between close
  revisions, where two-thirds of layout matches are byte-identical anyway; potentially reckless
  across unrelated binaries.
- **Whether the DOL-only limit binds.** `match` analyzes the DOL and ignores RELs. Fine for Prime,
  whose only module is small, but worth checking before another project relies on it.
- **String anchoring has more to give.** It produced only 18–38 anchors per run, yet on `R3ME01_00`
  those anchors are much of what seeds propagation, and strings survive recompilation and porting
  better than anything else measured. Currently a string must be referenced by exactly one function
  on each side; pairing on *sets* of shared strings would likely widen this considerably.

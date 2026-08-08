# Symbol matching: planned work

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

After the length guard, **no genuinely wrong match remains in the confident tier on any pair**. Every
confident "error" left is ground-truth noise: truncated names, `CARDStat`/`CardStat` capitalisation
drift, and `__arraydtor$381`/`__arraydtor$159` compiler-generated suffixes. On the hardest pair all
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

## Open questions

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

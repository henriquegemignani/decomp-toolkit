# Symbol matching: learnings

Working notes from building and tuning [`dtk match`](../README.md#match). Where `match_plan.md`
(if present) says what's left to do, this says what was learned getting here — the wrong turns,
the surprising measurements, and the reasoning that isn't obvious from the code alone. All
measurements are against the Metroid Prime decompilation, which has several versions of the same
executable and is the driving use case; specific numbers will drift as the algorithm changes, but
the shape of each finding should hold.

## The governing constraint

**An absent symbol name is better than a wrong one.** A wrong name reads as established fact,
propagates into decompiled source and onward into further version ports, and nothing prompts
anyone to re-check it. This one preference reshaped the design more than any accuracy number did:
it's why matches are tiered instead of thresholded, why `--renames` only ever carries confident
matches, and why the collision guard in the rename applier skips-and-reports instead of asserting a
name is free. Any future change to the scoring or tiering should be checked against this before
being checked against a precision number — a small accuracy gain that blurs the confident/candidate
boundary is a bad trade.

## Architecture, briefly

The pipeline: `CallGraph::build` (`src/analysis/callgraph.rs`) derives caller/callee edges from
relocations already resolved by `Tracker`, `fingerprint_all` (`src/analysis/fingerprint.rs`)
computes a relocation-masked instruction hash plus string references per function, and
`match_functions` (`src/analysis/matching.rs`) does the matching in two tiers:

1. **Anchors** — functions matched by content alone: identical name, unique masked-instruction
   hash, or a string literal referenced by exactly one function on each side.
2. **Propagation** — grows outward from anchors along call-graph edges (call-site position, sole
   remaining caller) and link order (position between two anchors), voting each round and
   committing only mutual, unambiguous winners.

The result is then classified into three tiers (`Match::tier()`) — confident / probable /
candidate — based on the *kind* of evidence, not a cut on the confidence score. `dtk match` reads
two project configs, `dtk symbols rename` applies a rename file to a `symbols.txt` in place.

## Findings that changed the design

### Confidence is not comparable across binary pairs

The single biggest realization: the same confidence number means different things depending on how
similar the two binaries are.

- Between close revisions (e.g. two NTSC builds a patch apart), layout-vote confidence is *tightly
  bimodal* — matches land at either ~0.867 (same opcodes and reference counts, different operands)
  or 0.98 (byte-identical after masking), with almost nothing between. A threshold barely
  discriminates anything.
- Between distant binaries (e.g. the NTSC source against the Wii Trilogy port), confidence spreads
  *broadly* across 0.56–0.98, and there the threshold actually separates good matches from bad
  ones.

This is why `Match::tier()` classifies on evidence kind (identical encoding, corroboration count,
runner-up margin) rather than on `confidence` directly. `confidence` is still computed and reported
— it's useful within one run, and `--min-confidence` still gates what's considered at all — but it
is not the thing tiering is built on. If you're tempted to simplify tiering into `confidence >= X`,
re-derive X for a distant pair and a close pair separately first; they won't agree.

### The identical-body promotion needed the same length guard as the hash anchor

`anchor_by_exact_hash` (tier 1) has always required `instruction_count >= MIN_DISTINCTIVE_INSTRUCTIONS`
(4), with the reasoning that short functions collide by coincidence — a bare `blr`, or
`li r3, 0; blr`, hashes identically against every other such stub in the binary, so "these two hash
the same" carries zero information below some minimum size.

The promotion path in `tier()` — a propagated match whose bodies hash identically gets promoted to
confident even though the hash wasn't globally unique — did not have this guard when first written.
It reused the raw `exact_hash == exact_hash` comparison. Found by review, not by testing: a stub
`blr` matched on a single layout vote could reach `confident` purely because its trivial body
happened to hash the same as its (correct, or incorrect) counterpart.

This was not theoretical once measured. Adding the shared guard (`is_distinctive`, now used by both
the anchor and the promotion) moved thousands of matches per close-revision pair out of `confident`
into `probable`/`candidate`, and it eliminated the one genuinely wrong confident match found in
testing before the fix existed: `__close_console` matched to `__write_console` — two console stub
functions with short, identical bodies, matched on layout position alone.

**Lesson**: when the same kind of evidence (`exact_hash` equality) is used to decide two different
things (an anchor vs. a promotion), any guard on one needs to be checked against the other. They
drifted here because the promotion path was added later in a different code path
(`accept_votes`/`confidence_for`) than the original anchor (`anchor_by_exact_hash`), and nothing
forced them to share the constant until it was pulled out explicitly.

There's also a degenerate case worth remembering: `CallGraph::build` only admits symbols with
`size >= 4` as nodes, so a genuinely zero-size function can't reach this path. But a node whose
section lookup fails, or whose section has no data at that range (e.g. bss), still produces a valid
`Fingerprint` with `instruction_count == 0` and `exact_hash` equal to the digest of zero bytes —
two such nodes are then "byte-identical" by the same comparison. The length guard closes this too,
incidentally, since `0 < MIN_DISTINCTIVE_INSTRUCTIONS`.

### A rename set must tolerate a name appearing more than once

First version of the rename applier (`src/util/renames.rs`) treated two source functions being
assigned the same target name as a hard parse error. This looked obviously correct — a name should
identify one symbol — and broke on the second real test run: `dtk match` producing a rename set
from a real one-to-one function matching, applied via `dtk symbols rename`, failed with:

```
'construct<10CModelData>__4rstlFPvRC10CModelData' is already assigned to 'fn_800CBB34'.
```

Checking the source project's own `symbols.txt` showed the same name at two different addresses,
`scope:local`, one per translation unit. Local template instantiations legitimately repeat their
mangled name across translation units — the source binary itself has 12 such duplicate function
names. A rename set derived from matching two versions of that same binary correctly reproduces
this: it isn't a bug in the matcher, and refusing it drops correct renames.

Fixed by making `Renames::insert` allow two entries assigning the same target name, rather than
rejecting them. What's still refused: renaming the *same source symbol* twice to different names
(that's actually contradictory), and — in `apply_renames`, not `Renames::parse` — assigning a name
that's already held by a symbol not itself being renamed away, since that actually would collide two
live symbols under one name.

**Lesson**: before treating any "this shouldn't happen" case as a hard error in code that consumes
real compiler output, check whether the compiler itself already does it. C++ name mangling
collisions across translation units (local template instantiations, anonymous namespaces before
they're disambiguated, etc.) are exactly the kind of thing that looks like corrupted input but is
actually normal.

### Project-relative paths don't work when a command spans two projects

Every other dtk command resolves a project config's relative paths (`object_base`, `symbols`,
`splits`, ...) against the process working directory — fine when a command operates on one project,
since you either run from the project root or pass `-C`. `match` takes two configs, which may be in
different repositories entirely (the eventual cross-game use case), so `-C` has nothing to point at
that serves both.

First approach considered: rewrite the paths inside the loaded `ProjectConfig` to be absolute before
using them. Abandoned — `object_base` and friends are `Utf8UnixPathBuf`, and joining a Windows
absolute path (`C:\Users\...`) onto that type silently produces something with the drive letter
lost (`/Users/henri/...`), which then fails to open with a confusing "not found" rather than an
obviously-wrong-path error. `ExtractConfig` and other nested config structs also carry more paths
than are obvious from a first read, so this approach means keeping a rewrite list in sync with the
config schema by hand.

What worked: reproduce "run from the project root" literally. `load_analyzed` resolves each
config's root (working directory if it already works there, else walk up from the config file
looking for what `object_base`/`object` names, else an explicit `--source-root`/`--target-root`),
then `WorkingDirectory::enter` changes into it for the duration of that one project's load, restoring
the previous directory via `Drop` — including on an early return through `?`. This is why the two
configs are loaded fully sequentially rather than in parallel (`source_analyzed` finishes, including
its guard's `Drop`, before `target_analyzed` starts): the working directory is process-global state,
so only one project's root can be "entered" at a time.

**Lesson**: don't rewrite path fields defensively when the actual constraint is "code downstream
assumes CWD == project root." Reproducing that assumption directly (enter/restore) is less code and
doesn't need to track every path field a config type happens to carry.

### Propagation needs far more rounds for distant binaries, and the cap failing was silent

The propagation round cap started at 24 (`MatchOptions::default`), sized against close-revision
pairs where propagation converges in 2–9 rounds. Running against the Wii Trilogy port —
structurally the same source but a much smaller, differently-organized overlap — hit the cap at
exactly 24 without an explicit signal; the only symptom was a suspiciously round final match count.
Each propagation round only advances the matched frontier by one call-graph hop, and a pair with
few anchors and a sparse initial matched set needs many more hops to reach everything reachable.

Raised the default to 100, exposed `--max-rounds`, and — more importantly — added an explicit
`log::warn!` when the loop exits via the cap rather than via "a round found nothing," since that
distinction (converged vs. truncated) is invisible from the match count alone and was the whole
reason the bug went unnoticed initially.

**Lesson**: any iterate-to-fixpoint loop with a safety cap needs to say out loud when the cap was
the reason it stopped, not just when the natural fixpoint was reached. Silent truncation looks
identical to a correct small result.

## Measurement discipline that mattered

**`--validate` (hide the target's real names, match anyway, then score against the hidden names)
is the only ground truth available**, and even that thins out fast: a version pair with only a few
hundred pre-existing names to hide leaves a correspondingly small scored sample, wide enough
confidence intervals that a "97%" and a "100%" aren't necessarily distinguishable. Treat any
single-pair percentage from a small `--validate` run as indicative, not exact, and prefer looking at
the *list* of disagreements over the summary percentage — see the next point.

**Apparent errors are usually ground-truth noise, not matcher errors**, and checking this by hand
was worth more than any amount of aggregate statistics. Recurring shapes found by reading actual
mismatches:
- Truncated names in an older symbols file (compare full name length, not just the visible prefix).
- Human capitalization/spelling drift between manually-named versions (`CARDStat` vs. `CardStat`,
  `CShockWave` vs. `CShockwave`).
- Compiler-generated suffix drift (`__arraydtor$381` vs. `__arraydtor$159` — same function, the
  numeric suffix isn't semantic).
Before concluding a tier's precision is below what its intended use requires, pull the actual
"incorrect" rows and read them; the number of *genuine* errors is consistently much smaller than the
raw mismatch count suggests.

**Multi-source consensus was investigated as a coverage strategy and found to be primarily useful
for something else.** Simulated (without building the feature) by running independent `dtk match`
invocations from different source versions against the same target and joining the reports on
target address:
- Coverage gain from combining sources was small (order 5%), and only from sources on genuinely
  independent axes. Two near-identical source versions (e.g. two NTSC patch revisions) add *zero*
  combined coverage over either alone — they anchor on the same content and propagate the same way,
  so agreement between them proves nothing.
- Precision value was the real finding: independent routes to the same target agreed on the
  overwhelming majority of shared matches, and where they *disagreed*, those disagreements were
  concentrated exactly on the kind of adjacent-same-class confusion (e.g. two sibling methods on the
  same class) that's hardest to catch any other way. This is planned as the mechanism behind the
  `Candidate` tier eventually meaning "evidence disagreed" instead of just "evidence was thin" — see
  [match_plan.md](match_plan.md), task 2.

**Test on an isolated copy, not the real project directory**, when validating anything that writes
files (`symbols rename`). Every destructive-path test in this work was run against a
scratch copy of the relevant `config/` directories, diffed against the untouched original
afterward, and the copy deleted once confirmed. This caught the shared-name bug (see above) without
ever risking the user's actual `symbols.txt`.

## Things worth re-checking before extending this further

- `MatchOptions::use_layout` has no CLI flag yet. Link-order inference assumes both binaries keep
  the same translation-unit layout, which holds within one game's revisions and will not hold
  across different games — the eventual second target for this tool. It needs to be exposed and
  probably default differently once source and target no longer share a codebase-and-linker
  lineage.
- The propagation vote weights, the tier thresholds (`CONTESTED_MARGIN`, `CORROBORATION_FOR_PROBABLE`,
  `MIN_DISTINCTIVE_INSTRUCTIONS`) were tuned against Metroid Prime's specific binary characteristics
  (PowerPC, Metrowerks CodeWarrior, this codebase's particular mix of templated STL-alike code).
  They are constants, not learned, and there's no reason to expect them to transfer unchanged to a
  structurally different binary — re-measure with `--validate` before trusting them elsewhere.
- Every "done" measurement in this document and in any plan doc was taken against a specific build
  of `dtk` and a specific commit of the target decompilation project. If either the scoring formula
  or the underlying `symbols.txt` files move, the numbers are stale — re-run rather than cite them
  as current fact.

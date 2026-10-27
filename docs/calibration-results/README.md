# Calibration run artifacts

One TOML per `calibrate grid` / `calibrate eval` run: the accepted `[priors]`,
the headline metrics and the warnings the search raised, wrapped in `#` comment
headers. These files are the calibration record — prose write-ups of the same
numbers drift from them, so nothing here is retold.

Every run writes its artifact to `calibration_results/` under the working
directory (`parser-api/` by default). The files kept in this folder are a
curated subset, so a run stays here only if it changed something or is the
measurement a decision rests on.

Reading one:

- the header carries the mode, split, N and NDCG@20 / Recall@50 / MRR;
- `[priors]` is the vector the search accepted — a complete `Priors` block, so
  it can be fed straight back into
  `calibrate eval --fit-mix=<this file>` for a paired re-score on a single
  matrix;
- `# WARN` lines are boundary hits (a knob stopped at its search clamp) and
  `# NOTE` lines are decisions the search made deliberately, such as switching
  a channel off by probe. A boundary hit means "the optimum may be outside the
  search range", not "the value is right".

See [`../calibration.md`](../calibration.md) for the harness, the flags, the
sweep modes and the caveats on holding out an artifact before copying numbers
into `config.example.toml`.

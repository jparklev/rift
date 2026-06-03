# Auto-Research Workflow For `rift create`

This workflow measures attempts to improve `rift create` performance against one large real workload, such as a Linux source checkout. The base checkout named `rift` remains unchanged, runs the comparison, and serves as the baseline. Experimental sibling workspaces named `rift-*` begin from that revision and receive different implementation ideas.

## What Exists

`create.rs` measures the production `Manager::create` path for one Rift implementation:

```bash
cargo bench --bench create -- /path/to/linux --samples 10 --output /path/to/result.json
```

`compare.rs` runs that benchmark for multiple Rift implementation checkouts and ranks them by median elapsed time:

```bash
cargo bench --bench compare -- /path/to/linux \
  --candidate /path/to/rift \
  --candidate /path/to/rift-git \
  --candidate /path/to/rift-registry \
  --samples 10 \
  --output /path/to/results/round-01
```

## Requirements

- Keep the base `/path/to/rift` checkout unchanged; run the comparison command from there and include it as the baseline candidate.
- Every candidate checkout must contain the `create` benchmark target and its dependencies. Create candidates from a revision containing this benchmark framework.
- All candidates should begin from the same Git revision before experimental changes are made.
- Name experimental sibling checkouts `rift-*`, such as `rift-git` or `rift-registry`.
- Use one workload directory for every candidate in a comparison round.
- Store output outside the workload directory and outside candidate checkouts.
- On Linux, the workload must be on supported btrfs or native reflink-capable storage for production copy-on-write behavior.
- On macOS, results measure APFS `clonefile`, not either Linux filesystem backend.

The benchmark initializes the workload before timing. On the first run on Linux, initialization of an ordinary btrfs directory may convert it into a subvolume before any measured samples are taken. Linux reflink initialization verifies native reflink support without conversion. It leaves the workload initialized for later rounds.

## Setup

First commit or otherwise preserve the benchmark framework in the base `rift` checkout, then create isolated candidate code workspaces from that same revision. A useful first-round layout is:

```text
/path/to/rift                unchanged baseline and benchmark runner
/path/to/rift-git            one Git-path hypothesis
/path/to/rift-registry       one registry hypothesis
/path/to/rift-strategy       one filesystem-strategy hypothesis
/path/to/rift-other          one additional measured idea
/path/to/linux               workload checkout
/path/to/results             benchmark evidence, outside all checkouts
```

The candidate suffixes are suggestions only. Each `rift-*` experimental candidate should test one clearly stated idea, so measured results can be attributed to a specific change.

For example, after the benchmark framework has been committed in the base checkout, create the experimental candidates with Rift itself:

```bash
cd /path/to/rift
rift init --here .
rift create --name rift-git --into /path/to
rift create --name rift-registry --into /path/to
rift create --name rift-strategy --into /path/to
rift create --name rift-other --into /path/to
```

Use `/path/to/rift` as the unchanged baseline and give the printed `rift-*` candidate paths to the agent for experimental changes. If Rift's storage configuration produces different locations than the example paths, use the paths printed by `rift create`.

## Establish A Baseline

Before changing candidate code, run the unchanged baseline alone:

```bash
mkdir -p /path/to/results/baseline
cd /path/to/rift
cargo bench --bench create -- /path/to/linux \
  --samples 10 \
  --output /path/to/results/baseline/create.json
```

The JSON result contains:

```json
{
  "benchmark": "create",
  "platform": "macos",
  "source": "/path/to/linux",
  "samples_ms": [37.8, 38.1, 37.6],
  "median_ms": 37.8,
  "min_ms": 37.6,
  "max_ms": 38.1,
  "cleanup_passed": true
}
```

Use `median_ms` as the initial comparison metric. Keep the raw samples to identify noisy results.

## Run An Auto-Research Round

Give the agent a workload, a fixed baseline candidate, experimental candidates, and a measurement budget. For example:

```text
Run an auto-research round to reduce median `rift create` time without breaking correctness.

Baseline and runner: /path/to/rift
Workload: /path/to/linux
Results: /path/to/results/round-01
Candidates:
- /path/to/rift-git
- /path/to/rift-registry
- /path/to/rift-strategy
- /path/to/rift-other
Samples per candidate: 10
Rounds: 1

Keep `/path/to/rift` unchanged and include it as the baseline in every comparison. Use one distinct hypothesis per `rift-*` candidate. Run `cargo test --workspace --locked` in every changed candidate before benchmarking. Exclude failing candidates. Run the comparison benchmark, inspect `summary.json`, and report the measured winner, patch summary, risks, and recommended next round.
```

For each experimental candidate, the agent should:

1. Inspect the current implementation path relevant to its hypothesis.
2. Make only the candidate-specific code change.
3. Run correctness verification inside that candidate:

```bash
cargo test --workspace --locked
```

4. Preserve the candidate if it passes, or mark it failed and exclude it from the comparison command.

After implementation and tests, execute the comparison from the unchanged base checkout:

```bash
cd /path/to/rift
cargo bench --bench compare -- /path/to/linux \
  --candidate /path/to/rift \
  --candidate /path/to/rift-git \
  --candidate /path/to/rift-registry \
  --candidate /path/to/rift-strategy \
  --candidate /path/to/rift-other \
  --samples 10 \
  --output /path/to/results/round-01
```

Remove any candidate from this invocation if its tests failed.

## Results

The comparison output directory contains:

```text
/path/to/results/round-01/
  candidate-01.json
  candidate-02.json
  candidate-03.json
  candidate-04.json
  candidate-05.json
  summary.json
```

`candidate-NN.json` contains the raw samples and aggregate timings from one candidate. `summary.json` maps each result back to its candidate checkout and ranks all included candidates by `median_ms`:

```json
{
  "benchmark": "create",
  "source": "/path/to/linux",
  "samples": 10,
  "candidates": [
    {
      "rank": 1,
      "candidate": "/path/to/rift-registry",
      "result": "/path/to/results/round-01/candidate-03.json",
      "median_ms": 34.9,
      "min_ms": 34.1,
      "max_ms": 36.0,
      "difference_from_fastest_percent": 0.0
    },
    {
      "rank": 2,
      "candidate": "/path/to/rift",
      "result": "/path/to/results/round-01/candidate-01.json",
      "median_ms": 39.4,
      "min_ms": 38.8,
      "max_ms": 40.6,
      "difference_from_fastest_percent": 12.9
    }
  ]
}
```

## Decision Rules

Use these rules when choosing what to do next:

1. Reject any candidate whose required tests fail.
2. Reject benchmark results where cleanup fails.
3. Prefer candidates with a lower `median_ms` than the unchanged `/path/to/rift` baseline.
4. Treat very small differences as inconclusive when their raw samples overlap substantially; rerun finalists with more samples.
5. Inspect code risk and scope before promoting a timing winner.
6. If two winning changes are independent, combine them in a fresh `rift-*` candidate created from `/path/to/rift`, then benchmark the combined candidate against each individual winner and `/path/to/rift`.
7. Keep each round's results directory unchanged as experiment evidence.

## Follow-Up Round

After a first round, create a new results directory and compare the most promising paths again. For example:

```bash
cd /path/to/rift
cargo bench --bench compare -- /path/to/linux \
  --candidate /path/to/rift \
  --candidate /path/to/rift-registry \
  --candidate /path/to/rift-combined \
  --samples 30 \
  --output /path/to/results/round-02-finalists
```

Use higher sample counts for finalists rather than spending that budget on clearly unsuccessful first-round candidates.

## Current Boundaries

The benchmark framework handles measurement, JSON result persistence, and candidate ranking. The agent currently handles hypothesis selection, parallel code changes, correctness checks, candidate exclusion, result review, and decisions about combining or continuing experiments.

This is sufficient for measured auto-research rounds. Automate more orchestration only after several rounds reveal repetitive work worth encoding.

<!-- graft-benchmark-report -->
## Graft performance

Fixed `ci` dataset, 4 aligned base/candidate pairs after 1 warmup pair(s). Negative paired change is better for every metric.

### Speed

| Metric | Baseline | Candidate | Paired change | Paired MAD |
|---|---:|---:|---:|---:|
| Repository init | 4.22 ms | 4.19 ms | ⚪ -2.6% | 7.1% |
| Stage initial dataset | 955.88 ms | 977.75 ms | ⚪ +4.2% | 25.0% |
| Commit initial dataset | 520.48 ms | 482.13 ms | ⚪ -4.0% | 3.6% |
| Stage 10% row update | 539.73 ms | 525.38 ms | ⚪ -1.5% | 1.4% |
| Commit incremental update | 501.34 ms | 541.47 ms | 🔴 +5.5% | 2.5% |
| Row diff between commits | 557.77 ms | 613.24 ms | ⚪ +9.5% | 7.5% |
| Checkout parent revision | 589.10 ms | 544.21 ms | 🟢 -9.2% | 2.3% |
| Push to filesystem remote | 646.71 ms | 657.29 ms | ⚪ +2.5% | 3.5% |

### Storage

| Metric | Baseline | Candidate | Paired change | Paired MAD |
|---|---:|---:|---:|---:|
| Worktree dataset | 9.85 MiB | 9.85 MiB | ⚪ +0.0% | 0.0% |
| Materialized SQLite database | 5.60 MiB | 5.60 MiB | ⚪ +0.0% | 0.0% |
| .graft after initial commit | 9.66 MiB | 9.66 MiB | ⚪ +0.0% | 0.0% |
| .graft after incremental commit | 16.97 MiB | 16.97 MiB | ⚪ +0.0% | 0.0% |
| Incremental history growth | 7.31 MiB | 7.32 MiB | ⚪ +0.0% | 0.0% |
| Initial storage amplification | 0.981× | 0.981× | ⚪ +0.0% | 0.0% |
| Two-commit storage amplification | 1.723× | 1.723× | ⚪ +0.0% | 0.0% |
| SQLite snapshot store | 10.62 MiB | 10.62 MiB | ⚪ +0.0% | 0.0% |
| Repository objects | 366.34 KiB | 366.34 KiB | ⚪ +0.0% | 0.0% |
| External file payloads | 6.00 MiB | 6.00 MiB | ⚪ +0.0% | 0.0% |
| Refs, index, and metadata | 805 B | 805 B | ⚪ +0.0% | 0.0% |
| .graft file count | 101 | 103 | ⚪ +2.0% | 0.0% |
| Repository object file count | 74 | 74 | ⚪ +0.0% | 0.0% |
| Filesystem remote after push | 16.47 MiB | 16.47 MiB | ⚪ +0.0% | 0.0% |
| Remote segments | 10.10 MiB | 10.10 MiB | ⚪ +0.0% | 0.0% |
| Remote storage commits | 920 B | 920 B | ⚪ +0.0% | 0.0% |
| Remote repository objects | 373.74 KiB | 373.74 KiB | ⚪ +0.0% | 0.0% |
| Remote external payloads | 6.00 MiB | 6.00 MiB | ⚪ +0.0% | 0.0% |
| Remote refs and metadata | 86 B | 86 B | ⚪ +0.0% | 0.0% |
| Remote file count | 11 | 11 | ⚪ +0.0% | 0.0% |

Baseline: `graft-sdk-v0.3.4` (`graft-tool 0.11.0`) · Candidate: `next` (`graft-tool 0.11.0`). Change is the median of aligned per-pair percentages; noise is their median absolute deviation. Storage uses apparent file bytes.

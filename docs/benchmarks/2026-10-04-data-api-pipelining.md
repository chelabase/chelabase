# Data API pipelining benchmark

## Setup

- Date: 2026-10-04 (UTC)
- Harness commit: `be4aa94`
- Neon ref: `956332507a7be94a00db47f0813412e1fa6d5b0d`
- Compute image: `ghcr.io/chelabase/neon-compute-v17:956332507a7b-dev@sha256:a41ebfec1bbea3892e1a6da09bd62b1baef925a9f1e13ef5a71b70bb165f8706`
- Compute image ID: `sha256:a41ebfec1bbea3892e1a6da09bd62b1baef925a9f1e13ef5a71b70bb165f8706`
- Netem image: `nicolaka/netshoot:v0.16@sha256:b09d9b21381f47a79b3cbcb30da25266dc17186ea00ae65e99fdc51396f48e70`
- Pool size: 32
- Sanity ping p50 (SELECT 1): 0 ms RTT: 0.21 ms; 0.5 ms RTT: 0.83 ms; 2 ms RTT: 2.43 ms
- Plan: full (latency: 200 warm-up + 2000 measured requests per variant and op, 3 repeats, variants interleaved with the first rotated per round; sanity: 200 pings per RTT; throughput at 0 ms RTT: 3 rounds of 32 concurrent, 2 s warm-up + 20 s measured per cell, a fresh pool per cell, variant order rotated per round, the table vacuumed before each round, median reported)

## Notes

- **RTT labels are netem settings, not measured round trips.** The sanity ping above is the real round trip: the "0.5 ms" step measured 0.83 ms (about 0.62 ms added to a 0.21 ms baseline), and the "2 ms" step 2.43 ms. Every variant goes through the same qdisc, so comparisons within a step are fair, but the 0.5 ms verdict was judged at about 0.8 ms.
- The harness and the compute run on one machine, so 0 ms throughput includes client-side CPU contention for every variant alike.
- Variants: A is today's sqlx path (rest's pool options: ping on acquire, `BEGIN`, preamble, queries, `COMMIT`, reset after release); B is sqlx with every cheap tweak (no ping, reset + `BEGIN` + `set_config` in one simple query); C is one tokio-postgres pipeline including `COMMIT`, with the schema-version guard and response cap in SQL; D is a pipeline that awaits the preamble before `COMMIT`. Details: `docs/superpowers/specs/2026-10-04-data-api-pipelining-design.md`.

## Latency at 0 ms RTT (p50 / p95 / p99, ms)

| op | A | B | C | D |
|---|---|---|---|---|
| ByKey | 1.19 / 1.74 / 2.35 | 0.87 / 1.25 / 1.64 | 0.43 / 0.63 / 0.92 | 0.66 / 0.97 / 1.30 |
| List100 | 1.43 / 2.09 / 2.95 | 1.11 / 1.58 / 2.04 | 0.67 / 0.94 / 1.22 | 0.87 / 1.25 / 1.60 |
| Insert | 2.44 / 3.52 / 4.69 | 2.08 / 3.00 / 3.96 | 1.63 / 2.41 / 3.41 | 1.82 / 2.65 / 3.52 |
| Batch | 3.36 / 4.65 / 5.89 | 3.00 / 4.07 / 5.14 | 1.99 / 2.74 / 3.58 | 2.18 / 3.07 / 3.99 |

## Latency at 0.5 ms RTT (p50 / p95 / p99, ms)

| op | A | B | C | D |
|---|---|---|---|---|
| ByKey | 3.96 / 4.74 / 5.65 | 2.51 / 3.04 / 3.50 | 0.95 / 1.18 / 1.43 | 1.74 / 2.12 / 2.51 |
| List100 | 4.19 / 5.07 / 5.87 | 2.73 / 3.30 / 3.79 | 1.16 / 1.45 / 1.73 | 1.93 / 2.34 / 2.76 |
| Insert | 5.38 / 6.60 / 8.28 | 3.83 / 4.84 / 5.83 | 2.23 / 3.12 / 4.18 | 2.96 / 3.87 / 5.15 |
| Batch | 7.19 / 8.67 / 10.06 | 5.69 / 6.96 / 8.37 | 2.51 / 3.35 / 4.45 | 3.16 / 4.20 / 5.37 |

## Latency at 2 ms RTT (p50 / p95 / p99, ms)

| op | A | B | C | D |
|---|---|---|---|---|
| ByKey | 11.81 / 12.72 / 13.77 | 7.19 / 7.85 / 8.59 | 2.51 / 2.77 / 3.06 | 4.86 / 5.33 / 5.80 |
| List100 | 12.03 / 13.05 / 14.01 | 7.41 / 8.06 / 8.99 | 2.74 / 3.05 / 3.45 | 5.03 / 5.48 / 6.06 |
| Insert | 13.12 / 14.59 / 16.57 | 8.42 / 9.58 / 11.72 | 3.66 / 4.54 / 6.29 | 6.02 / 7.03 / 8.64 |
| Batch | 18.27 / 19.93 / 22.06 | 13.62 / 15.12 / 16.86 | 3.98 / 5.08 / 6.60 | 6.35 / 7.49 / 8.89 |

## Throughput at 0 ms RTT (requests/s, median of 3 rounds)

| op | A | B | C | D |
|---|---|---|---|---|
| ByKey | 2048.7 | 2796.3 | 4408.2 | 3365.9 |
| List100 | 1192.7 | 1337.2 | 1300.8 | 1382.5 |
| Insert | 1633.0 | 2181.2 | 2429.0 | 2625.6 |
| Batch | 792.4 | 836.1 | 1004.6 | 1036.6 |

## Verdict (D1, against A)

- B: FAIL
  - Insert at 0.5 ms: p50 drops 28.9% (< 30%)
  - Insert at 0.5 ms: p99 drops 29.6% (< 30%)
  - Batch at 0.5 ms: p50 drops 20.8% (< 30%)
  - Batch at 0.5 ms: p99 drops 16.9% (< 30%)
  - Insert at 2 ms: p99 drops 29.3% (< 30%)
  - Batch at 2 ms: p50 drops 25.4% (< 30%)
  - Batch at 2 ms: p99 drops 23.6% (< 30%)
- C: PASS
- D: PASS

## C vs D (the spec's rule)

- C beats D's p50 by ≥20% at 0.5 ms on the read ops (ByKey, List100): yes (ByKey: C 0.95 ms vs D 1.74 ms, 45.5% lower; List100: C 1.16 ms vs D 1.93 ms, 39.8% lower)

## Shipped path (variant R)

Phase 2 built C into `rest`. Variant R runs each op through `rest::exec::Engine::query` (the code rest's HTTP handler calls: request parse, database pick, pool, cached owner-level check, `chelabase_query::run` pipelined), built on the internal source, as `authenticated` with the same fixed claims, so the measured path is rest's request path minus HTTP. R returns the same JSON as C (live test `variants_agree`). Run: `just bench-data-api --rtts-ms 0.5,2 --variants C,R --repeats 2 --no-throughput` (200 warm-up + 2000 measured requests per op and repeat, C and R interleaved, a fresh database, harness commit `c6cbbbe`; the sanity ping p50 was 0.86 ms at the 0.5 ms setting and 2.41 ms at the 2 ms one). Both pools connect without TLS.

| op | RTT (ms) | C p50 (ms) | R p50 (ms) | R vs C |
|---|---|---|---|---|
| ByKey | 0.5 | 0.88 | 0.92 | +4.0% |
| List100 | 0.5 | 1.08 | 1.12 | +3.4% |
| Insert | 0.5 | 2.08 | 2.14 | +3.0% |
| Batch | 0.5 | 2.46 | 2.53 | +3.1% |
| ByKey | 2 | 2.46 | 2.51 | +1.7% |
| List100 | 2 | 2.72 | 2.79 | +2.7% |
| Insert | 2 | 3.59 | 3.66 | +2.0% |
| Batch | 2 | 3.93 | 4.01 | +1.9% |

Verdict: PASS. R's p50 is within ±10% of C's at both RTTs for every op (+1.7% to +4.0%): the shipped path keeps C's one-round-trip latency, and its extra work (parsing, the cached lookups, the response-size accounting) costs about 40 to 80 microseconds per request.

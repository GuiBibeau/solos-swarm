# Off-peak swarm results, 2026-10-27 (a6 + a6r prop AMM, b6 + b6r funding carry)

Runs: a6 (20 tasks, 9 filed / 11 partial, $10.68), a6r (10/10, $8.16), b6 (11 filed / 9 partial, $38.84), b6r (8 filed / 1 partial, $25.44). Total about $83. The 19 lost tasks in a6/b6 were the 403 "exceeds Goal limit" bug (context window advertised as 1M; fixed in 60056f9); a6r/b6r reran them with zero 403s. Full artifacts: `~/.local/state/solos-swarm/runs/<run>/artifacts/final.json` on the box.

## Prop AMM (a6 + a6r): no edge, but a concrete build plan for a Phoenix MM
- Public material is exhausted. Program IDs verified for AlphaQ, GoonFi, Aquifer, ZeroFi, HumidiFi, SolFi/V2, Obric V2, BisonFi and Quantum. Quote layouts are unknown for all of them; nothing can be priced without captured account bytes.
- Quote updates are cheap (485-676 CU against at least 16,938 for a swap; 1.3-20 SOL/day of priority fees). Update cost is not the moat. Ordering within the slot is.
- HumidiFi rewrites its quote about 17.5 times per slot (expected age about 11 ms at a random taker arrival). AlphaQ is slow (about 0.5 s) and so the most pick-off-able, but also small (0.7% share).
- Counterparty premium: GoonFi charges non-aggregator flow +3.59 bps; ZeroFi does not widen (-0.05 bps).
- Replay is harder than assumed: at least 99.8% of prop swaps are CPIs, the Jupiter test kit needs a top-level instruction, and a quote depends on signer set, caller, CU budget, clock and flow type. Jupiter licence 3.2(g) bars combining API content with captured content, so we cannot validate our ladders against Jupiter quotes.
- Capture cost: LaserStream charges 2 credits per message, which makes the old estimate wrong (18.7M-124M credits/day for 9 SOL-USDC markets). Per-slot getMultipleAccounts polling is about 345.6k credits/day (54x cheaper). A replay older than about 20 minutes returns finalized data only. Do not buy a gRPC tier on the old numbers.
- Recommendation from the swarm: a 14-day SOL-USDC quote-capture pilot, polling not gRPC, with 5 kill criteria, run T01/T02 (market counts by dataSize, slot time) first.

## Funding carry (b6 + b6r): the carry edge is not from carry
- XPL funding gap vs CEX flips sign on val: -6.1 bps/day on fit/select, +5.1 on val, across Binance, Bybit and Hyperliquid. It is a Phoenix mark-vs-oracle basis effect (R2 0.76 against the Phoenix basis, 0.003 against Binance funding).
- The referee (b6r) on frozen candidate A:
  - cleared: funding sign convention, funding-interval stability within the window;
  - bounded: look-ahead (+0.06 correlation), overlap inflation (x1.12 day-block, x1.67 symbol-cluster; only about 15 independent 7-day blocks);
  - material: survivorship (only BTC/ETH/SOL have full history; that subsample is about 16 bps weaker) and split leakage (5% of dev trades reach into val; select is interleaved in fit).
- **Carry-only proxy of candidate A is -61 bps/trade** over 419 trades. The whole +8.8 bps gross edge comes from the price/hedge legs, not funding. The forward test must separate these; the swarm's kill rule: dead if the forward 90% CI upper bound is below 0, or if carry-only stays below -40 bps while the hedged total is positive.
- Capacity: near-zero cross-market correlation, but the book caps at about $4k of capital.
- Do not retune A. Test a basis-gated variant as a separate candidate with its own single look.

## Swarm architecture fixes (adopt in the next run)
Add raw/.md twins to seed URLs (the 120 KB fetch cap returns 0% of HTML article text); pre-submit contract lint (36/50 records dropped `description`); a shared verified-constants block with a numeric self-check; a negative-result record type; a numeric tool-call budget with "file early"; coordinators should not get run_code; self-contained proofs (33 records pointed at ephemeral /work paths).

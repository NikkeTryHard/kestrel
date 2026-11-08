# Kestrel

Rust bot for the ScrabbleBot auction game on SpacetimeDB. Each auctioned letter is
valued by Monte Carlo over sampled future racks, priced against a fitted rival bid
model and a budget-pacing multiplier, and played out through an exact search for the
best set of disjoint words. No neural network, Python, or external model is used at
runtime.

## Build

```sh
cargo build --release --locked -p kestrel
```

The binary is `target/release/kestrel`.

Native competition build on Linux with Clang and mold:

```sh
CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
CARGO_PROFILE_RELEASE_OPT_LEVEL=3 CARGO_PROFILE_RELEASE_DEBUG=0 \
RUSTFLAGS='-C target-cpu=native -C linker=clang -C link-arg=-fuse-ld=mold' \
cargo build --release --locked -p kestrel
```

This targets the build machine's CPU; use the portable build on other hardware.

## Run

```sh
./target/release/kestrel --token-file /absolute/path/to/bot.token run
```

Run from the repository root so `config/` is found. `run` joins casual games and
registers for any open tournament; add `--no-tournaments` for casual-only play.
`kestrel status` prints the bot identity, rating and tournament state. Run one
process per credential.

## Layout

| Crate | Contents |
| --- | --- |
| `kestrel-words` | Alphabet, racks, dictionary, exact word-partition solver |
| `kestrel-market` | Rival bid model, field priors, tunable parameters |
| `kestrel-policy` | Valuation, bidding and word-play policy |
| `kestrel-net` | SpacetimeDB bindings and blocking connection helpers |
| `kestrel` | CLI, match engine, telemetry, replay |

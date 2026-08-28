<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo/herd-mark-dark.svg">
    <img src="assets/logo/herd-mark-light.svg" alt="herd" width="150">
  </picture>
</p>

# herd

A minimal game, used only to generate load for [umwelt](../umwelt-rs). Never
published to crates.io.

Three binaries, matching the three a umwelt consumer writes:

- `herd-sim` owns a `WorldSimulation`, supplies the game, and serves a region.
  Its own population — attractors, residents and nomads, crowding damage, a
  spawner — lives alongside whatever the edges bring.
- `herd-edge` holds an `EdgeServer`: game clients over QUIC on one side, regions
  over NATS on the other. With `--to` and `--migrate` it also walks a herd of
  its own between two regions.
- `herd-game` is a game client. It speaks no NATS and knows no region except the
  ids that come back on its own entities.

`herd-common` holds what they share: argument parsing, the world all three agree
on, and the two QUIC endpoints a deployment has to build for itself.

```
nats-server &
cargo run --release -p herd-sim -- 0 --region 7
cargo run --release -p herd-edge
cargo run --release -p herd-game -- --region 7 --observers 256
```

`umwelt` is a path dependency on a sibling checkout, so herd sees the public API
and nothing else. What herd cannot do through that API is a finding about the
API rather than a reason to reach around it. Two of those are already recorded
in `herd-common`: umwelt's default payload budget of 1200 bytes does not fit a
QUIC datagram once the edge's header is in front of it, and a client has to name
the region it is spawning into because an edge has no home.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo/herd-mark-dark.svg">
    <img src="assets/logo/herd-mark-light.svg" alt="herd" width="150">
  </picture>
</p>

# herd

A minimal game, used only to generate load for [umwelt](../umwelt-rs). Never
published to crates.io.

Two binaries, matching the two a umwelt consumer writes:

- `herd-sim` owns a `WorldSimulation`, supplies the game, and drives the tick
  loop.
- `herd-edge` will own a `SimulatorEdge`. Placeholder until umwelt has one.

`umwelt` is a path dependency on a sibling checkout, so herd sees the public API
and nothing else. What herd cannot do through that API is a finding about the
API rather than a reason to reach around it.

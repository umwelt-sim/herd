# herd logo

The sibling of umwelt's mark: the same crowd of entities, with no observer and
no view radius, since herd supplies load rather than watching it. One member
carries the accent.

## Which file goes where

| Surface | File | Notes |
| --- | --- | --- |
| GitHub repo avatar | `png/herd-avatar-512.png` | square; GitHub applies its own rounding |
| GitHub repo social preview | `herd-social-preview.png` | 1280x640, the size GitHub documents as best display, under its 1MB limit |
| Browser tab | `favicon.svg`, `favicon.ico` | the SVG follows the viewer's color scheme |
| README, light background | `herd-mark-light.svg` | transparent |
| README, dark background | `herd-mark-dark.svg` | transparent |

Optical sizes and palette match umwelt. The generator lives in the umwelt
checkout at `umwelt-rs/assets/logo/generate.py` and writes these files here;
there is one source of geometry rather than two that can drift.

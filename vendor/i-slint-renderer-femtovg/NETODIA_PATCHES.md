# Netodia patch to Slint 1.16.1

Source: the published `i-slint-renderer-femtovg` 1.16.1 crate (checksum in the
upstream registry: `72b28a8f0a7a777e670fbe2af6cbcec4935c37335bc0dab100f73bdc197dfa3a`).
Upstream sources and licence notices are preserved. Netodia takes the GPL-3.0 arm.

`FemtoVGRenderer::register_font_from_memory` wraps the `&'static [u8]` in
`fontique::Blob::new(Arc::new(data))` instead of copying it through `to_vec()`.
The allocation owns a slice reference; the font bytes stay in the executable's
static storage. Fontique and Parley retain their normal ownership, font metadata,
glyph coverage and variable font support. No unsafe code or public API changes.

The six embedded faces total 18,286,304 bytes. This removes their duplicate heap
buffers; it does not remove resident executable pages, font metadata, glyph
atlases or graphics driver allocations. The whole-process saving must be measured.

Remove this directory and its root manifest patch once the upstream renderer
registers static fonts without copying. The upstream `Cargo.toml` is the registry's
normalized manifest so this patch introduces no dependency changes.

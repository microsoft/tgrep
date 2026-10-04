# Local patch to ignore 0.4.25

This directory contains the existing locked `ignore` crate, not a version
upgrade. All upstream files are preserved; the only upstream source change is
in `src/dir.rs`. The upstream commit's root `rustfmt.toml` is also included
so workspace formatting preserves the vendored source's original style.

- Source: <https://crates.io/crates/ignore/0.4.25>
- Archive: `ignore-0.4.25.crate`
- Original SHA-256:
  `d3d782a365a015e0f5c04902246139249abf769125006fbe7649e2ee88169b4a`
- Upstream commit: `57c190d56eedac90c061a238b63dbfed434fee50`,
  `crates/ignore` in <https://github.com/BurntSushi/ripgrep>.
- Upstream licenses: `LICENSE-MIT`, `UNLICENSE` and `COPYING`, unchanged.

## Parser delta

`resolve_git_commondir` reads the first gitfile/commondir line as bytes instead
of a UTF-8 `String`. A small converter preserves native path bytes on Unix and
retains UTF-8 validation on other platforms. CRLF handling, error propagation,
ignore-source precedence and nested repository behavior are unchanged.

The upstream parser rejects valid Unix metadata paths containing non-UTF-8
bytes. That prevents a synchronized worktree from reaching readiness even
after repository identity validation succeeds. Ignoring the error or disabling
`info/exclude` would silently change coverage, so neither is used.

The real-Git shared-daemon regressions cover native gitfiles and independently
native commondir files, full CLI/RPC lifecycle, global/info-exclude/gitignore/
dot-ignore precedence, nested repositories, malformed ignore errors, and
filesystem-scan parity. The upstream crate tests remain included.

## Distribution

`tgrep-core` depends directly on this directory, without a registry-version
fallback. This keeps the patch in normal workspace builds, Git/path consumers,
the separate fuzz workspace, `cargo install --path tgrep-cli --locked`, and
the repository's checkout-based release/cross-build pipelines. It does not
depend on a consumer honoring a workspace-only `[patch.crates-io]`.

This repository distributes binaries and source checkouts, not crates.io
packages. Cargo packaging/publishing cannot silently replace the patch with
unmodified registry `ignore`: the path-only dependency causes an explicit
missing-version error. Publishing crates would require a deliberately released
patched dependency first. Remove this local copy only after an equivalent
upstream release passes the same native-path and ignore-precedence regressions.

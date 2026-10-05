# vendor/

Third-party crates that are checked in because the workspace cannot resolve
without a change to them. Each one is the published source, unmodified except
for what is called out below, and is wired in through `[patch.crates-io]` in the
root `Cargo.toml`.

## `ironrdp-connector`

Published `ironrdp-connector` 0.10.0, with exactly one line changed:

```diff
-picky = "=7.0.0-rc.25"
+picky = "=7.0.0-rc.26"
```

Why it has to be here:

- `smb` (the SMB probe) requires `sspi` **exactly** `=0.21.3`.
- Published `sspi` 0.21.3 pins macOS/iOS-only release candidates of
  `curve25519-dalek`, `ed25519-dalek` and `p256`/`p384`/`p521` which no
  published `russh` agrees with, so that combination does not resolve. The
  `[patch.crates-io] sspi` rev is the last commit still declaring 0.21.3 and it
  already moves those pins to the stable releases.
- That rev requires `picky =7.0.0-rc.26`, while `ironrdp-connector` 0.10.0
  requires `=7.0.0-rc.25`. Two release candidates of the same version can't
  coexist in one dependency graph, and neither requirement can be satisfied from
  outside the crates themselves.

Delete this directory (and its `[patch.crates-io]` entry) as soon as IronRDP
publishes a release built against picky rc.26 — nothing else in the tree depends
on the copy.

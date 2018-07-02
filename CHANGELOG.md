# tokio-pty-process 0.2.0 (2018 Jul 2)

- Updated to depend on the 0.2 series of `tokio_signal` which makes it possible to
  use the 0.1 series of `tokio`, rather than the deprecated `tokio-core`. Thanks
  to `@aep` for [the patch](https://github.com/pkgw/stund/pull/1).

# 0.1.3 (2018 May 27)

- Switched to
  [tokio-serde-bincode](https://crates.io/crates/tokio-serde-bincode) rather
  than [tokio-serde-json](https://crates.io/crates/tokio-serde-json). The
  former is published on `crates.io`, which allows *us* to be published on
  `crates.io`.

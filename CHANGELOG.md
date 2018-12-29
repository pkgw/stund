# stund 0.1.4 (2018 Dec 29)

- Update dependencies, including using `tokio-pty-process` 0.3.1. This should
  get us working on FreeBSD! We also have started using `tokio-codec` instead
  of the deprecated codec types from `tokio-core.`

# tokio-pty-process 0.3.1 (2018 Dec 29)

- Get this crate working on FreeBSD (same fix as macOS; thanks to
  `@fabianfreyer` for [the patch](https://github.com/pkgw/stund/pull/44).

# stund_protocol 0.1.4 (2018 Dec 29)

- Start using `tokio-codec` rather than the deprecated codec types in
  `tokio-core`.
- Start using `dirs::home_dir` rather than the deprecated `std::env::home_dir`.

# stund 0.1.3 (2018 Oct 10)

- Update dependencies, including using `tokio-pty-process` 0.3.0. This should
  get us working on macOS!

# tokio-pty-process 0.3.0 (2018 Oct 10)

- **API breakage**: the previous method `CommandExt::spawn_pty_async` now opens the
  PTY in cooked, not raw, mode. A new method, `CommandExt::spawn_pty_async_raw`, opens
  it in raw mode. Change inspired by a contribution by [@aep](https://github.com/aep).
- Get this crate working on MacOS, hopefully. Thanks to `@aep` for
  [the patch](https://github.com/pkgw/stund/pull/21).

# tokio-pty-process 0.2.0 (2018 Jul 2)

- Updated to depend on the 0.2 series of `tokio_signal` which makes it possible to
  use the 0.1 series of `tokio`, rather than the deprecated `tokio-core`. Thanks
  to `@aep` for [the patch](https://github.com/pkgw/stund/pull/1).

# stund 0.1.2 (2018 May 27)

- Switched to
  [tokio-serde-bincode](https://crates.io/crates/tokio-serde-bincode) rather
  than [tokio-serde-json](https://crates.io/crates/tokio-serde-json). The
  former is published on `crates.io`, which allows *us* to be published on
  `crates.io`.

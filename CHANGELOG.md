# stund 0.1.6 (2019 Jun 4)

- Update dependencies, including using `tokio-pty-process` 0.4.0. This doesn't
  add any new features as far as `stund` is concerned, but gets us using the
  new version of that crate that provides the `PtyMaster` trait.

# tokio-pty-process 0.4.0 (2019 Jun 4)

- Add and implement the `PtyMaster` trait, allowing basic controls of PTY
  characteristics. (`@fabianfreyer`,
  [#50](https://github.com/pkgw/stund/pull/50)).

# stund 0.1.5 (2019 Apr 17)

- Update dependencies, including using `tokio-pty-process` 0.3.2. This should
  get us working on FreeBSD for real this time!
- Fix a bug related to the updated dependencies that broke `stund`
  functionality on `master` for a while. Ooops. (`@pkgw`,
  [#71](https://github.com/pkgw/stund/pull/71)).

# tokio-pty-process 0.3.2 (2019 Apr 17)

- *Actually* get this working on FreeBSD. We need to set the flags on the PTY
  master after opening it. Thanks to `@fabianfreyer` for
  [the patch](https://github.com/pkgw/stund/pull/48).
- Added FreeBSD CI and a test case that should hopefully help with getting
  these things right in the future (`@pkgw`,
  [#70](https://github.com/pkgw/stund/pull/70)).
- Add a feature that makes it possible to “split” an `AsyncPtyMaster` into halves,
  analogous to what can be done with generic Tokio read/write streams. We implement
  the functionality ourselves so that we can additionally make it possible to
  implement `AsRawFd` for the split halves. Thanks to `@fabianfreyer`.

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

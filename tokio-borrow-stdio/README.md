# tokio-borrow-stdio

“Borrow” the standard input/output streams temporarily, exposing them to a
Tokio core as nonblocking streams.

There are several crates implementing this general class of functionality:

- [tokio-file-unix](https://crates.io/crates/tokio-file-unix)
- [tokio-stdin-stdout](https://github.com/vi/tokio-stdin-stdout)
- [tokio-stdin](https://crates.io/crates/tokio-stdin)
- [tokio-stdio](https://github.com/smith61/tokio-stdio)

Why yet another one? The key is in the name — this is the only crate that
*temporarily* takes over stdin/stdout. This makes it possible for the
[stund](https://github.com/pkgw/stund) CLI client to temporarily start up an
asynchronous Tokio core when necessary, but otherwise use traditional blocking
I/O for most of its user interactions.

This crate only works on Unix-like operating systems. (The program for which
it was developed, [stund](https://github.com/pkgw/stund), uses things like
pseudo-TTYs and Unix domain sockets, so there is no expectation of ever
porting it to other kinds of OS.)

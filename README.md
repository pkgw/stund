# stund — an SSH tunnel daemon

Be warned, this README is very much a work in progress!

Stund (“stunned”), an SSH tunnel daemon, will maintain SSH tunnels in the
background for you. It is convenient when you are often logging in to


## Usage

Probably all you ever need to run is:

```
stund open login.mydomain.org
```

This will more-or-less run `ssh login.mydomain.org` in such a way that the
command will disconnect from your terminal after you type in your passwords.
*If you use
[SSH connection multiplexing](https://en.wikibooks.org/wiki/OpenSSH/Cookbook/Multiplexing)*,
subsequent SSH connections to `login.mydomain.org` will reuse the
pre-authenticated connection, avoiding repeated password entry.

If you don't use SSH connection multiplexing, stund is basically pointless.

If you would normally give SSH more arguments when connecting to the your
host, set up your
[SSH config file](https://en.wikibooks.org/wiki/OpenSSH/Client_Configuration_Files)
with the necessary entries. Virtually any option that appears on the command
line can be automated through SSH configuration.

Other stund commands:

```
stund close login.mydomain.org # close an existing tunnel
stund status # report status of tunnels
stund exit # shut down the background daemon if it's running
```

(Yes, stund is basically like running SSH in a
[GNU screen](https://www.gnu.org/software/screen/) session, with a slightly
nicer user experience. And in writing it I got to learn a lot of exciting
things about psuedoterminals and asynchronous I/O with Rust’s
[Tokio](https://tokio.rs/) framework.)


## Installation

For now, you have to compile stund yourself:

1. Install the [Rust language](https://www.rust-lang.org/en-US/) toolchain if
   you don't already have it.
2. Add `$HOME/.cargo/bin` to your `$PATH` if it is not already there.
3. Check out this repository.
4. Run `cargo install` in the toplevel directory.
5. Run `stund help` to verify the installation.


## Copyright and License

Stund is copyright its authors and is licensed under the
[MIT License](https://opensource.org/licenses/MIT).

# stund — an SSH tunnel daemon

Stund (“stunned”), an SSH tunnel daemon, will maintain SSH tunnels in the
background for you. It is convenient when you are often logging in to remote
systems that require you to type in a password every time you connect.

[![](http://meritbadge.herokuapp.com/stund)](https://crates.io/crates/stund)

If you have a [Rust](https://rust-lang.org/) toolchain available, you can
install `stund` by running

```
cargo install stund
```

[See below](#installation) for more detailed installation instructions.


## Usage

Probably all you ever need to run is:

```
stund open login.mydomain.org
```

This will more-or-less run `ssh login.mydomain.org` in such a way that the
command will disconnect from your terminal after you type in your passwords.
**If you use
[SSH connection multiplexing](https://en.wikibooks.org/wiki/OpenSSH/Cookbook/Multiplexing)**,
subsequent SSH connections to `login.mydomain.org` will reuse the
pre-authenticated connection, avoiding repeated password entry.

If you don't use SSH connection multiplexing, stund is basically pointless.

If you would normally give SSH more arguments when connecting to the your
host, set up your
[SSH config file](https://en.wikibooks.org/wiki/OpenSSH/Client_Configuration_Files)
with the necessary entries. Virtually any option that appears on the command
line can be automated through SSH configuration. **You should probably set
`ServerAliveInterval = 120` for tunnels to be maintained with stund.**

Other stund commands:

```
stund close login.mydomain.org  # close an existing tunnel
stund status                    # report status of tunnels
stund exit                      # shut down the background daemon
```

(Yes, stund is basically like running SSH in a
[GNU screen](https://www.gnu.org/software/screen/) session. But the user
experience is a bit nicer, and in writing it I got to learn a lot of exciting
things about psuedoterminals and asynchronous I/O with Rust’s
[Tokio](https://tokio.rs/) framework.)

The `open` command can optionally exec another command after it finishes, if
you run it with the following syntax:

```
stund open login.mydomain.org -- command arg1 arg2
```

This can be useful as a one-liner to open a needed tunnel and log into a host
lying behind a gateway:

```
stund open login.mydomain.org -- ssh -J login.mydomain.org myinnerhost
```

If the connection to `login.mydomain.org` does not need any user interaction
to be opened, the explicit invocation of `stund` can be avoided with a proper
SSH `ProxyCommand` configuration item, as mentioned below.


## Installation

For now, you have to compile stund yourself. But, installing the latest
version should be pretty simple:

1. Install the [Rust language](https://www.rust-lang.org/en-US/) toolchain if
   you don't already have it. In almost all cases the recommended method is to
   use [rustup.rs](https://rustup.rs/).
2. Add `$HOME/.cargo/bin` to your `$PATH` if it is not already there.
3. Run `cargo install -f stund`
4. Run `stund help` to verify the installation.

You don’t need to check out this repository unless you want to install
a bleeding-edge version of `stund` rather than the latest release.


## Things You Can Do With Multiplexed SSH Tunnels

- If you log into a service that requires you to type your password, you
  can just type it once in the morning, instead of periodically throughout
  the day as you accidentally close your “primary” connection!
- [Open and close port forwards dynamically](https://en.wikibooks.org/wiki/OpenSSH/Cookbook/Multiplexing#Port_Forwarding_After_the_Fact).
- [Transparently log in to hosts that are inside gateways](https://en.wikibooks.org/wiki/OpenSSH/Cookbook/Proxies_and_Jump_Hosts#Jump_Hosts_--_Passing_Through_a_Gateway_or_Two),
  so that you can do things like `scp` files without having to make multiple
  hops.

If you need to log into hosts that live behind a gateway, *and* the gateway
doesn’t require any user interaction for you to login in successfully, you can
use stund’s “exec-after-open” functionality to automatically open long-lived
background SSH tunnels with `ProxyCommand` settings that look like this:

```
Host inner.mydomain
ProxyCommand = stund open --no-input -q login.mydomain.org -- ssh -W inner:%p login.mydomain.org
```

The `--no-input` option is needed to prevent `stund` from trying to read
anything from standard input; otherwise it would consume some of the SSH
traffic.


## Things Stund Can’t Do

The big limitation is that `stund` can’t keep your SSH connection alive if you
suspend your laptop or switch networks. It’s simply not possible to do this
due to the fundamental design of the SSH protocol (namely, that SSH runs over
long-lived TCP connections).

If this is the functionality you want, the best solution of which we are aware
is [mosh](https://mosh.org/), which uses a sessionless UDP-based protocol
that’s bootstrapped over a temporary SSH connection. While this design allows
`mosh` to overcome some of SSH’s limitations, it means that `mosh` doesn’t
support features like port forwarding and file transfers. Also, `mosh`
requires bidirectional UDP traffic between the client and the server, which is
often disallowed by conservative firewall rules.


## Changes

See [CHANGELOG.md](./CHANGELOG.md) for a list of changes associated with each release.


## Copyright and License

Stund is copyright its authors and is licensed under the
[MIT License](https://opensource.org/licenses/MIT).

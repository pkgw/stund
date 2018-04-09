# stund — an SSH tunnel daemon

Be warned, this README is very much a work in progress! As is the project as a
whole.

Stund (“stunned”), an SSH tunnel daemon, will maintain SSH tunnels in the
background for you. It is convenient when you are often logging in to remote
systems that require you to type in a password every time you connect.


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


## Installation

For now, you have to compile stund yourself:

1. Install the [Rust language](https://www.rust-lang.org/en-US/) toolchain if
   you don't already have it.
2. Add `$HOME/.cargo/bin` to your `$PATH` if it is not already there.
3. Check out this repository.
4. Run `cargo install` in the toplevel directory.
5. Run `stund help` to verify the installation.


## Things You Can Do With Multiplexed SSH Tunnels

- If you log into a service that requires you to type your password, you
  can just type it once in the morning, instead of periodically throughout
  the day as you accidentally close your “primary” connection!
- [Open and close port forwards dynamically](https://en.wikibooks.org/wiki/OpenSSH/Cookbook/Multiplexing#Port_Forwarding_After_the_Fact).
- [Transparently log in to hosts that are inside gateways](https://en.wikibooks.org/wiki/OpenSSH/Cookbook/Proxies_and_Jump_Hosts#Jump_Hosts_--_Passing_Through_a_Gateway_or_Two),
  so that you can do things like `scp` files without having to make multiple
  hops.


## Copyright and License

Stund is copyright its authors and is licensed under the
[MIT License](https://opensource.org/licenses/MIT).

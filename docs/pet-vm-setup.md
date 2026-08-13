# Running aj on a pet VM

Reference material for a long-running `aj serve` behind an `aj gateway`,
the setup the remote-control work is driven on day to day. The unit files
are in `deploy/`. aj does not install them, and nothing here is
automated: provisioning is a later phase, this is the hand-built version
it will eventually reproduce.

These are **systemd user units**, and the repos live in your own home
directory. Both choices matter, see below.

## The shape

One host per working directory. That is not a convention, it is what a
host is: a host serves the sessions of the directory it was started in,
and its `host_id` names that store (spec section 4). Two checkouts means
two units and two ports.

A gateway aggregates hosts behind one address and namespaces their
sessions as `<host_id>:<session_id>`. It holds no logs and no cursors, so
losing it costs the clients their connection and nothing else.

## Why user units, and why repos under home

**User units** run as you, with your environment, your `~/.aj`, your ssh
agent and your git identity. A system unit would need a service account
that then has to be given a checkout, credentials and a git identity of
its own, and every one of those is a thing to get wrong. `systemctl
--user` is also unprivileged, so a stuck host is yours to restart without
sudo.

**Repos under `$HOME`** is what keeps two hosts apart. A session store is
named after the working directory: under home it is the path *relative to
home* with the separators turned into dashes, so `~/projects/api` becomes
`projects-api` and `~/archive/api` becomes `archive-api`. A directory
outside home falls back to its last component alone, so `/srv/a/api` and
`/srv/b/api` would both be `api` and share one store. `host-id` lives in
that store, so hosts colliding this way also report the same `host_id`,
and one id is one namespace, so a gateway will not name the second one at
all. The symptom is a host that runs fine, answers `hello`, and never
appears in the directory.

Keeping repos under home makes that essentially impossible. The one
remaining ambiguity is a literal dash: `~/dev/project` and
`~/dev-project` both resolve to `dev-project` (`aj-conf`'s `paths` module
documents this and tolerates it).

## Before anything listens

The control port runs arbitrary commands through the agent. Serving it
unauthenticated publishes a remote shell, so the gate is not optional
(spec section 6.11):

- **`--auth tailscale`** verifies every peer against the local tailscale
  daemon and admits only the logins you name with `--allow`, or a tagged
  node granted `github.com/aljoscha/aj/cap/control` in the tailnet policy.
  A tagged node has no login, so the capability is the only way to admit
  one. This is what the reference units use.
- **`--auth local`** (the default) serves loopback only, and refuses to
  start on any other address rather than serving unauthenticated. With an
  ssh tunnel this needs no tailnet.
- **`--auth open`** belongs only to a network that is private by
  construction, such as an ember guest reachable from its hypervisor and
  nowhere else (spec section 7.4). A machine you can ssh into is not
  that.

Draft the tailnet policy against the real tailnet before the first host
listens, not after.

## Install

```sh
sudo loginctl enable-linger "$USER"     # keep user units running with no login session
install -D -m755 target/release/aj ~/.local/bin/aj

mkdir -p ~/.config/systemd/user
cp deploy/aj-serve.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now aj-serve
```

`enable-linger` is the one easy thing to forget: without it your units
stop when you log out and start when you log in, which looks exactly like
a crash loop tied to your ssh sessions. It needs `sudo` on a machine you
reach over ssh, where enabling it even for your own account is a
privileged action.

Edit `WorkingDirectory`, `AJ_ALLOW` and the listen address first. For a
second host, copy the unit under a second name with its own
`WorkingDirectory` and port:

```sh
sed -e 's|%h/projects/api|%h/projects/web|' -e 's|6161|6162|' \
    deploy/aj-serve.service > ~/.config/systemd/user/aj-serve-web.service
```

Host and gateway both default to `127.0.0.1:6161`, so anything sharing a
machine needs its address said out loud. The reference units put the
gateway on 6160 for that reason, not because the protocol cares.

Credentials live in `~/.aj/.env`, readable by you and nobody else.

## The gateway

```sh
cp deploy/aj-gateway.service ~/.config/systemd/user/
systemctl --user enable --now aj-gateway
```

Static hosts go in `~/.aj/gateway.toml`, which the gateway reads if it is
there and does not mind if it is not:

```toml
hosts = ["100.64.0.10:6161", "100.64.0.11:6161"]
```

So write it whenever you like, before or after the first start, and
restart the unit to pick it up. Hosts enrolled at runtime over
`/v1/hosts` need no file at all: those enrollments persist under
`~/.aj/gateway/` and come back on their own.

The unit deliberately passes no `--config`. Naming a file explicitly makes
it required, so a `--config` pointing at one that is not written yet is a
hard failure, and with `Restart=on-failure` that is a restart loop rather
than a message. Only use the flag for a file somewhere other than the
default path.

## Checking it

```sh
curl -s localhost:6161/v1/hello    # on a host
curl -s localhost:6160/v1/sessions # on the gateway, ids namespaced
curl -s localhost:6160/v1/hosts    # what the gateway thinks of each host
journalctl --user -u aj-serve -f
```

A host that answers `hello` but never appears in the gateway's directory
has two usual causes: the gate refusing the gateway, since the gateway is
a peer like any other and needs a login the host allows, or two hosts
sharing a `host_id` through a colliding store, which the gateway will not
give a second namespace. `/v1/hosts` tells you which: both read
`connected: false`, and the `error` on the row says whether the last
attempt was turned away by the gate or refused over its id.

Then connect a client:

```sh
aj connect http://gateway-host:6160
```

With more than one host enrolled, a create has to name the host it is for,
because a session runs an agent in that host's working directory and the
gateway will not guess (section 6.6 of the remote-control spec). In the TUI the
create action asks: it opens a picker over the enrolled hosts, with nothing
selected until you say so. A run with no terminal to ask names the host itself:

```sh
aj connect http://gateway-host:6160 --new --host 290dc828
```

The value is a host id, or any prefix of one that only a single host answers to
(`/v1/hosts` lists them). A value that fits none of them, or several, is refused
with the candidates listed rather than resolved to a guess.

## What a restart costs

Restarting a **host** ends its clients' streams and drops sessions that
were never persisted. A session with no content has no log, so it does
not survive. Clients re-attach with their cursors and resume
incrementally where the epoch survived. The `host_id` is stable across a
restart, since it lives in the session store rather than in the process,
so namespaced ids clients are holding keep resolving.

Restarting the **gateway** ends every client stream and drops its
knowledge of what each host holds. It relearns on reconnect. Learned host
ids persist, so a configured host that is down when the gateway starts is
still named in the directory, marked unreachable, with no rows under it
(spec section 7.1).

A **reboot** brings both back through linger, but a user unit cannot
order itself against system units, so a host may fail its first attempts
while the network or the tailscale daemon is still coming up. That is
`Restart=on-failure` doing its job, and `journalctl --user -u aj-serve`
will show the failed starts before the successful one.

That indefinite retry is deliberate, and it has one cost worth knowing.
A start that fails permanently, a bad path or a missing file, does not
end in `failed`: with `RestartSec=5s` the attempts are spaced wider than
systemd's default rate-limit window, so the unit retries forever and
`is-active` keeps answering `activating`. A unit stuck there is not
starting slowly, it is failing every time:

```sh
systemctl --user show -p NRestarts --value aj-serve   # climbing, not settling
journalctl --user -u aj-serve -n 20                   # the same error each time
```

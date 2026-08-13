# Running aj on a pet VM

Reference material for a long-running `aj serve` behind an `aj gateway`,
the setup the remote-control work is driven on day to day. The unit files
are in `deploy/`. aj does not install them, and nothing here is
automated: provisioning is a later phase, this is the hand-built version
it will eventually reproduce.

## The shape

One host per working directory. That is not a convention, it is what a
host is: a host serves the sessions of the directory it was started in,
and its `host_id` names that store (spec section 4). Two checkouts on one
VM means two units and two ports.

A gateway aggregates hosts behind one address and namespaces their
sessions as `<host_id>:<session_id>`. It holds no logs and no cursors, so
losing it costs the clients their connection and nothing else.

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
  start on any other address rather than serving unauthenticated. Combined
  with an SSH tunnel this needs no tailnet.
- **`--auth open`** belongs only to a network that is private by
  construction, such as an ember guest reachable from its hypervisor and
  nowhere else (spec section 7.4). A pet VM is not that.

Draft the tailnet policy against the real tailnet before the first host
listens, not after.

## Layout

```
/srv/aj/home/         HOME: sessions, config, credentials, gateway state
/srv/aj/workspace/    the checkout one host serves
/srv/aj/gateway.toml  static host addresses
/usr/local/bin/aj     the binary
```

`HOME` must persist. A fresh one is a new host with a new `host_id`, and
every namespaced session id a client is holding stops resolving.

## Install

```sh
useradd --system --home-dir /srv/aj/home --create-home aj
install -d -o aj -g aj /srv/aj/workspace

cp deploy/aj-serve.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now aj-serve
```

Edit `WorkingDirectory`, `AJ_ALLOW` and the listen address first. For a
second host on the same VM, copy the unit to a second name with its own
`WorkingDirectory` and port.

Host and gateway both default to `127.0.0.1:6161`, so anything sharing a
machine needs its address said out loud. The reference units put the
gateway on 6160 for that reason, not because the protocol cares.

Credentials go in `/srv/aj/home/.env`, readable by the `aj` user and
nobody else. They are the reason `HOME` is not world-readable and the
reason the gateway's unit gets the same treatment even though it runs no
agent itself.

## The gateway

```sh
cat > /srv/aj/gateway.toml <<'EOF'
hosts = ["100.64.0.10:6161", "100.64.0.11:6161"]
EOF

cp deploy/aj-gateway.service /etc/systemd/system/
systemctl enable --now aj-gateway
```

Hosts can also be enrolled at runtime over `/v1/hosts`, which persists
under `HOME`, so the file is for the ones that should come back after a
restart on their own.

## Checking it

```sh
curl -s localhost:6161/v1/hello    # on a host
curl -s localhost:6160/v1/sessions # on the gateway, ids namespaced
journalctl -u aj-serve -f
```

A host that answers `hello` but never appears in the gateway's directory
is usually the gate refusing the gateway: the gateway is a peer like any
other and needs a login the host allows.

Then connect a client:

```sh
aj connect http://gateway-host:6160
```

## What restarting costs

Restarting a **host** ends its clients' streams and drops sessions that
were never persisted. A session with no content has no log, so it does not
survive. Clients re-attach with their cursors and resume incrementally
where the epoch survived.

Restarting the **gateway** ends every client stream and drops its
knowledge of what each host holds. It relearns on reconnect. Learned host
ids persist, so a configured host that is down when the gateway starts is
still named in the directory, marked unreachable, with no rows under it
(spec section 7.1).

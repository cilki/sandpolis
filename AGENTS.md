Sandpolis is a Rust-based virtual estate manager that covers:

- Physical devices like servers, desktops, phones, etc
- Purely virtual entities like accounts, EC2 instances, etc

It's comprised of multiple applications:

- Server
- Agent
  - "Regular" mode
  - "UKI" mode
- Client
  - GUI based on Bevy
  - CLI based on clap for scripting or optional TUI based on Ratatui

All of these applications are built from the main `sandpolis` crate (except for
the mobile app) with feature flags. The agent's "UKI" mode is its own feature:
`uki` implies `agent` and builds the boot agent (always-on chainloader UI, cold
snapshot streams). It is a **compile error** to combine `uki` with `server` or
`client`, which means `--all-features` no longer builds — never use it; always
pass an explicit feature list like `--features server,agent,client`.

Every crate in the workspace apart from `sandpolis` and `sandpolis-mobile` is a
_subsystem_ that brings some functionality. Subsystems can depend on each other
and some are optional (controlled via cargo features).

Most subsystems implement functionality for all three instance types. For
example, the way to think about the `sandpolis-server` crate is it "implements
server-related functionality", not that it is itself the server. Therefore, it's
OK for the `sandpolis-client` crate to depend on `sandpolis-server` to get
shared types (just not with the `server` feature enabled).

A subsystem's runtime state lives in one or more _managers_ — `ShellManager`,
`DatabaseManager`, `NetworkManager`, `RealmManager` — which are constructed at
startup and held together in `InstanceState`.

A subsystem may also provide at most one _layer_, which is strictly a GUI
concept: the mode the client's layer picker chooses between, which decides node
visibility, the toolbar, and the node panel body. `LayerName` names it, and
because the mapping is one-to-one, a layer's name is its subsystem's name.
Notifications and services are attributed to a layer so the client can group
them.

#### Instances

An _instance_ is a Sandpolis process running as an **agent**, **server**, or
**client**. One process is exactly one of them, named by its subcommand:

```sh
sandpolis server   # server daemon
sandpolis agent    # agent daemon
sandpolis client   # client, in the foreground
```

#### Strata

Servers exist in one of two _strata_. Every network has **exactly one global
stratum (GS) server** and **any number of local stratum (LS) servers**.

An LS is an edge cache, useful for on-premise installations where it keeps
serving the instances around it even when the link to the GS is down. It
connects to exactly one GS, and never to another LS.

The distinction decides five things:

- **Configuration.** Only the GS reads _realm configs_ (`<realm>.realm.ron`),
  one per realm it serves, which it finds by scanning its `--data` directory; a
  realm exists only because a file declares it, and can never be created at
  runtime. Every other instance — LS servers, agents, clients — is configured by
  CLI flags plus the _realm cert_ (`<realm>.realm.pem`) naming the server it
  trusts.
- **Trust.** The GS holds the realm CA and is the network's single trust root.
  An LS never generates a CA; on first start it enrolls with the GS, which
  issues it a server certificate. The CA's private key never leaves the GS, so
  an LS can verify peers but never issue certificates of its own. Enrollment
  blocks the listener (with backoff) until it succeeds, since the certificate is
  what the listener presents.
- **Ownership.** Every piece of instance data has exactly one owner at a time:
  the server that instance is directly connected to. A server always owns its
  own scope; estate-wide data (users, accounts, realms) is always owned by the
  GS. The GS arbitrates through a persistent grant table: servers _claim_ their
  attached instances (the GS claims locally, an LS over a claim stream), each
  transfer bumps a fencing epoch, and disconnection is **not** a release — an LS
  keeps its scopes, and keeps writing, through a GS outage and across its own
  restarts. Ownership only moves when an instance shows up attached somewhere
  else.
- **Writability.** `RealmDatabase::write(scope)` gates every write on the
  ownership above: the GS holds full authority, an LS holds
  `WriteAuthority::Scoped`. A freshly granted scope is not writable until it has
  been _hydrated_ — fully replicated down from the GS — so its revision counters
  continue where the previous owner left off. The only paths around the gate are
  replication itself (revision-guarded, so an older record never clobbers a
  newer one) and instance-local bookkeeping (see `RealmDatabase::local_write`).
- **Routing.** Streams cross strata. A client addresses an agent by `InstanceId`
  and never learns the topology: an LS advertises its attached instances to the
  GS, and points its own default route at the GS for everything else.

```sh
# The global stratum server. It serves every realm config in its data
# directory, creating ./data/default.realm.ron if it finds none. A blank realm
# config means "generate a CA for me", which is written back into the file on
# first start.
sandpolis server --data ./data

# Every start also mints one realm cert per realm and writes it to
# <realm>.realm.pem in the same directory. That file is what attaches another
# instance: copy it where it's needed. Its common name is the realm's address,
# so it names exactly one server and realm.
cp ./data/default.realm.pem ./ops.realm.pem

# A local stratum server. The realm cert is how it authenticates to the GS in
# order to enroll, and having one is what puts this server in the local stratum,
# so it serves no realms of its own — realm configs in its data directory are
# ignored.
sandpolis server --realm ./ops.realm.pem --data ./ls-data --listen 0.0.0.0:8769

# An agent, attached to either stratum. Without --data it keeps nothing across
# restarts. --poll makes it check in on a schedule rather than staying
# connected.
sandpolis agent --realm ./ops.realm.pem --data ./agent-data \
          --poll '0 */5 * * * *' --poll-timeout 30

# A client, attached to either stratum. It picks up every realm cert in --data,
# so naming one is only necessary without a data directory. With neither it
# starts at the login dialog instead.
sandpolis client --realm ./ops.realm.pem
sandpolis client --data ./client-data
```

A realm cert is three PEM blocks — this instance's certificate, the realm CA
that verifies the server, and the private key — so it is readable by `openssl`
and assembled by hand if need be. `$S7S_REALM` is the environment alias for
`--realm`, `$S7S_DATA` for `--data`.

Replication always follows ownership, and it is always pull-based: the owner
serves, the replica subscribes. An agent attached to an LS has its records
applied to the LS's database (once the scope is granted and hydrated), and the
GS pulls them back up with one subscription per LS covering that server's owned
scopes plus its own. Estate-wide data flows the other way, down a standing
global-scope subscription. Because the GS only ever pulls an instance's records
from that instance's current owner, a stale owner's writes can never enter the
estate — the pull subscription is the fence.

#### Notifications

Any subsystem, on any instance, can tell the user something happened. The first
argument names the layer it's attributed to, which is how the client groups it:

```rust
notification::notify(
    Notification::error("Health", format!("{name} failed")).about(instance_id),
);
```

That writes a `NotificationData` row owned by the raising instance, so delivery
is just the replication above — agent to its owning server, up to the GS, out to
any client with the standing subscription. There is no notification protocol.

The client decides how the user finds out: an in-app toast while the window has
focus, the operating system's own notification interface when it doesn't (or
when there's no GUI at all, as in a TUI subcommand). A persisted watermark keeps
a subscription's opening snapshot from announcing history on every start.

#### World View

Users interact with the Sandpolis network via a real-time graph called the
_world view_. The graph is made of:

- Nodes
  - A _node_ is a point on the map. It could be an instance, probe, or a generic
    entity.
- Links
  - A _link_ expresses some general relationship between two nodes
- Terrains
  - A _terrain_ is a grouping of nodes

#### Domains

A _domain_ groups nodes under a shared name, which is drawn as a terrain. The
name is a service domain like `github.com`, so an instance and the accounts on
that service land in one region.

An account always belongs to one — its domain is part of its identity. An
instance does not: membership is an assignment stored in `DomainData`, owned by
the GS and replicated from there, and an instance no domain names belongs to
none and draws no terrain. Servers are never members, because domains group the
estate a server manages rather than the servers managing it.

## Development loop

Every time a server starts, it mints an endpoint certificate for each realm it
serves and writes it to `<realm>.realm.pem` in the data directory. Clients and
agents hold the same kind of certificate, so that one file attaches either.

A server started with no `--data` has no directory to scan, so it serves an
implicit `default` realm whose CA lives only in its in-memory database and its
certificate goes to `/tmp` instead. That's the whole setup for running the three
instances against each other on one host:

```sh
sandpolis server                                  # terminal 1
sandpolis agent  --realm /tmp/default.realm.pem   # terminal 2
sandpolis client --realm /tmp/default.realm.pem   # terminal 3
```

#### Containers

Every image comes out of `sandpolis/Dockerfile`, built with the repository root
as the context because `build.rs` walks the whole workspace:

```sh
docker build --target server -f sandpolis/Dockerfile -t sandpolis/server .
docker build --target demo   -f sandpolis/Dockerfile -t sandpolis/demo   .
```

The `server`, `agent` and `client` targets each carry one instance. `demo`
carries all three around a single `--features server,agent,client` binary
(remember `--all-features` doesn't build): its entrypoint starts a server, waits
for the realm cert, attaches an agent, and opens the GUI client if a wayland
socket or `$DISPLAY` was handed in. Without one it drops into a shell with
`$S7S_REALM` and `$S7S_DATA` already pointed at the running demo, so the client
subcommands work without flags.

Both stages are nix. The build runs inside `nix-shell shell.nix`, and the
runtime image is the closure of a `buildEnv` from the same package lists
(`nix/deps.nix`, shared with the dev shell) unioned with every store path the
binary itself names — so a library the dev loop needs can't go missing from an
image. `nix/nixpkgs.nix` pins nixpkgs for both, which is what makes the runtime
closure match the glibc the binary was linked against.

## Mobile App

The `sandpolis-mobile` crate wraps the main `sandpolis` crate.

Build instructions for Android:

```sh
cargo ndk -t arm64-v8a --platform 31 -o android/app/src/main/jniLibs build --link-libcxx-shared
cd android && ./gradlew assembleDebug
```

# Roadmap to 1.0

> This project has been in development for a long time and we need to rapidly
> move toward a MVP and then a stable 1.0 release afterwards. This roadmap
> outlines our overall requirements in no particular order.

- On desktop, probe, and shell layers: servers are present in the graph (so we
  have links), but they are not interactable. When the server layer is active,
  only servers are shown and they become interactable. Clients are only present
  in the graph when the client layer is active (servers are also present, but
  not interactable).
- Notifications currently only reach the user as a toast or an OS notification.
  Add a notification center in the GUI (history, per-layer muting). When the
  client is running in the foreground, show in-app toasts and no OS-native
  notification. If the client is not running in the foreground, only show
  OS-native notifcations.
  - Notification on errors

## `sandpolis-tunnel`

- **Direct (hole-punched) tunnels.** `TunnelMode::Direct` is a stubbed seam:
  `direct::attempt_direct` always fails, so a `Direct` client↔agent tunnel falls
  back to the indirect bridge seamlessly (`effective_mode` records the choice).
  Real NAT traversal needs a networking foundation that doesn't exist yet — a
  UDP transport, a DTLS `InstanceConnection` (the third transport its doc
  comment anticipates), and a STUN/TURN-style rendezvous (sketched in
  `direct::Rendezvous`, reworked from the long-dead `network::messages`).
- **Config hot-reload.** Tunnels are applied at server startup; the realm-config
  watcher does not yet re-orchestrate on edits (stop removed / start added).
- **UDP session expiry.** UDP sessions live for the tunnel's lifetime rather
  than expiring on idle, so churny UDP accumulates session ids until stop.
- **Multi-stratum control.** Only the global stratum server orchestrates (it is
  the only one that reads realm configs); an endpoint is reachable via the
  relay, but there's no LS-local bridging to keep same-LS traffic off the GS.
- Tunnel TUI widget (the CLI's no-subcommand form shows a placeholder).

## `sandpolis-agent`

- Provide prebuilt agent binaries for SSH deployment to install. The deploy
  framework resolves them through `deploy::binary::AgentBinarySource`, which
  currently has no source installed, so a fresh install stops with "no prebuilt
  agent binary available".

## `sandpolis-account`

- Allow CRUD operations on account objects
- Analyze attack surface
- Compromise tracing:
  - Suppose any entity in the network is compromised, what others could be
    affected?
  - Assign a weight on how bad a compromise of an entity would be
- Search for existing accounts with adler
  - Also augment with publically available information like account creation
    dates
- Service that checks accounts with haveibeenpwned API

## `sandpolis-snapshot`

- Boot-mode gating is now compile-time: the block stream responders only exist
  in `uki` builds, so regular agents never answer them. Remaining runtime work:
  the management responder should refuse (rather than hang on) an agent that
  isn't a boot agent
- Multi-stratum: the management responder refuses agents attached to a different
  (LS) server instead of forwarding to the owner
- Deleting non-leaf snapshots (requires a rebase/squash of the chain)
- Incremental layers are bigger than the actual change: `qemu-img convert -B`
  skips only zero-reading source clusters, it does not content-compare against
  the backing file, so unchanged non-zero clusters are stored again
  (zstd-compressed). The wire transfer is truly incremental; only the at-rest
  layer isn't. Verified correct both ways — a smarter commit would need
  qemu-nbd/qemu-storage-daemon writes into a fresh overlay
- A partition that changed size invalidates its chain; auto re-base instead of
  refusing
- `wipe_free` (zero free space while filesystems are mounted, shrinking later
  cold snapshots; useless on encrypted disks) exists in `agent.rs` but is not
  wired to any stream, service, or CLI
- Snapshot TUI widget (the CLI's no-subcommand form shows a placeholder)

## `sandpolis-health`

- Run DNS/TCP/HTTP tests

## `sandpolis-audit`

- auditd ingestion on agent, detection rules
- Button on toolbar that sets "Away" mode where monitoring becomes more strict
  - For example, a sucessful SSH login when away is highly suspicious and must
    be notified immediately
- Configurable notifications in realm config file
  - failed login attempts
  - all login attempts

## `sandpolis-probe`

- FTP(S) probe
- HTTP probe (`http.rs`)
  - Drive from health layer?
- RDP probe via desktop subsystem (`rdp.rs`, on the IronRDP crates)
  - Driven from the desktop layer, like VNC
  - First cut negotiates TLS security; hosts requiring NLA/CredSSP need
    `enable_credssp` turned on and the sspi CredSSP path exercised
  - `ironrdp-connector` is vendored under `vendor/` with a one-line picky bump
    (rc.25 to rc.26) so it resolves against the pinned `sspi` rev; every other
    IronRDP crate comes from crates.io. Drop it once IronRDP releases against
    picky rc.26
- RTSP probe (`rtsp/`)
  - Button on expanded node panel to maximize the video stream
  - Driven from the desktop layer?
- SSH probe via shell subsystem
  - Driven from the shell layer
- IPMI probe (skeleton in `ipmi.rs`, needs real BMC queries)
  - Drive from inventory layer?
- SNMP probe — partial, needs MIB-driven discovery
  - Drive from inventory layer?
- ARP probe (`arp/`) — verify completeness
- SMB probe (`smb.rs`) — `SmbFs` behind `crate::filesystem`, mirroring `NfsFs`.
  Its dependencies are pinned in two places, both worth understanding before
  touching them:
  - `smb` is `0.12.1` from crates.io, which builds on `sspi 0.21` so its crypto
    stack unifies with `russh`'s. Keep the `kerberos` feature off: it routes
    NTLM through SPNEGO, which Samba rejects. Its `sspi` requirement is exact
    (`=0.21.3`), which is what fixes the patch below.
  - `[patch.crates-io] sspi` points at the last upstream commit still declaring
    0.21.3. Published 0.21.3 pins exact release candidates of
    `curve25519-dalek`/`ed25519-dalek`/`p256` that no published `russh` agrees
    with — cargo cannot resolve both. Those pins are macOS/iOS-only and that rev
    has already moved them to the stable releases; delete the patch once there's
    a published `sspi` release that `smb` accepts.
  - `arp-toolkit`'s `sync` feature must stay off. It is `maybe-async/is_sync`,
    a feature of a shared proc-macro crate, so it switches every
    `#[maybe_async]` user in the graph — including `smb-transport`, whose tokio
    code then doesn't compile.
- Node panels on probes in probe layer just show what protocols are supported -
  to interact with probes, you use a more specific layer like desktop,
  filesystem, etc.

## `sandpolis-filesystem`

- GUI: delete, create folder, upload/download (the wire ops exist on both
  paths; the panel buttons don't drive them yet)
- Client can mount remote filesystems via FUSE
- Probe devices are browsed through `sandpolis_probe::filesystem`, a
  protocol-agnostic interface (list/stat/read/write/create_dir/remove/rename/
  statfs) that the probe subsystem implements per protocol. This layer never
  sees NFS or SMB. The panel currently drives only list/stat/statfs; the
  mutating operations are already on the interface for the TODOs above.
- Agents are browsed over `FsSessionStream` (`session.rs`): opening a node's
  panel opens one relayed session per agent, and a `List` arms a `notify`
  watcher on the agent so changes to the shown directory push fresh listings
  unprompted. Statfs resolves the disk containing the path via `sysinfo`.
  CreateDir/Remove/Rename are answered on the agent but nothing sends them.
- Both browsers render the listing as one bound label; clickable entries wait
  on the shared table widget growing row virtualization (client subsystem).

## `sandpolis-desktop`

- Button on expanded node panel to maximize the desktop session

## `sandpolis-instance`

- Improve the GUI for viewing instance databases
- Assign an instance's domain from the GUI

## `sandpolis-shell`

- SSH probes open a terminal here like agents (`ssh.rs`, `probe` feature).
  Telnet probes aren't modelled yet.
- The CLI/TUI (`sandpolis shell --instance`) can't target a probe; it would need
  a device flag on `TargetArgs`

## `sandpolis-inventory`

- Manage firmware updates

## `sandpolis-client`

- Virtualize rows in the shared table widget (`src/gui/ui/table.rs`) so large
  lists (e.g. installed packages) can use it

## `sandpolis` (main crate)

- Encrypted storage enclave for secrets
- Support direct connections between clients/agents if hole punching works
  - Streams can optionally run over this direct connection
- Whenever a stream is active, we need to render that in the GUI as a dotted
  line running parallel to the link between the nodes
  - This also works for streams running over direct connections
- The server places a "boot hold" per agent (`BootAgentData.hold`, written on
  the server; nothing toggles it from a client yet). A boot agent announces
  itself on the `BootStream` after connecting; the responder answers
  Hold/Proceed and sends Release when the flag clears, which reboots the agent.
  While held, a snapshot operation swaps the homepage for the snapshot layer's
  block-grid display (`boot_snapshot::active()`).
- Building the UKI itself (`mkrootfs`) remains; the UI uses
  `SLINT_BACKEND=linuxkms-noseat` there, winit on a desktop
- Also configured as fallback in case the primary OS fails to boot which the UKI
  detects (not implemented)
- See if we can generate better, more cohesive instance type icons under
  sandpolis-client/assets/network/ (these are what node panels show for
  instances)
- Implement db sync permissions
- Which nodes each layer shows, and the order they appear in the picker, are
  declared on each layer's `LayerClientInfo` and by `LAYER_ORDER` in
  `sandpolis-client/src/gui/ui/layer.rs`. The health layer now shows
  docker/libvirt probes (its panel discriminates on `ctx.target.sub` like
  shell/desktop/filesystem); ssh probes there are still pending. One probe
  filter is still missing:
  - The inventory layer should show probes that support ipmi/snmp
  - It's blocked on its panel, which reads `ctx.target.instance`. For a probe
    node that's the gateway server's id, so it'd report the server's inventory
    as the device's. Shell, desktop, filesystem and health discriminate on
    `ctx.target.sub` and route probe targets to per-protocol code; inventory
    needs the same before its nodes can appear.
- Revise how the realm config is updated
  - If a user modifies it, the server diffs the previous and reloads any
    subsystem that changed
  - If a user makes the config invalid, the change is ignored and the previous
    config is still used with a warning log
  - If a user makes a config change at the same time as the server, the user
    change has precedence and the server retries. If there's a collision in the
    config, the server aborts the change which should lead to an error message
    in the GUI.
- When rustls gets a working DTLS implementation, coordinate DTLS "connection"
  directly between agents and clients
- If there's only one domain, don't show a terrain

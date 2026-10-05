<p align="center">
	<img src="https://raw.githubusercontent.com/fossable/sandpolis/master/.github/images/sandpolis-256.png" />
</p>

![License](https://img.shields.io/github/license/fossable/sandpolis)
![Stars](https://img.shields.io/github/stars/fossable/sandpolis?style=social)

<hr>

`sandpolis` is a **virtual estate manager** which is a tool for controlling
esoterica like your online accounts, cloud servers, and even physical devices.

<p align="center">
	<img src="https://raw.githubusercontent.com/fossable/sandpolis/master/.github/images/overview.png" />
</p>

## Virtual estate

Virtual/digital estate is an encompassing
[term](https://www.fossable.org/fossable/virtual_estate) that refers to all
digital assets under your control. Some assets may be entirely virtual and
mostly controlled by a corporation, like accounts on _github.com_. Others have a
physical component as well, like a Raspberry Pi.

All of these entities are part of your _virtual estate_ and are intricately
connected in both obvious and unapparent ways.

As an example, you might have an SSH key or API token on your machine that
grants access to repositories (a digital asset) on Github. And suppose your
machine also has an authorized key installed that allows access from another
machine:

```
┌──────────┐ SSH Key ┌──────────┐ API Token ┌────────────────┐
│Machine A ┼─────────►Machine B ┼───────────► Github Account │
└──────────┘         └──────────┘           └────────────────┘
```

This picture represents a simple virtual estate with physical/digital assets
that you have a high degree of control over (local machines), and purely digital
assets that you have very little control over (an online account).

Sandpolis is about mapping out these relations to provide an overall view of
your entire virtual estate. It can do both microscopic management tasks (like:
"give me a shell on Machine A") and macroscopic tasks (like: "map out the attack
surface of my Github repos").

### Who cares about virtual estates anyway?

Whatever you call it, non-physical or digital assets have a significant impact
on our "real" lives. Sandpolis places all of those points on a map so you can
track them in one place, with the ultimate goal of uncovering who controls what
parts of your virtual estate.

Not everyone agrees on how much control we should personally have over our
virtual estates. Some people simply don't care - just put it all in the cloud.
Others recognize that the "cloud" is just someone else's computer and they're
effectively sharing control over their digital assets.

If you're in the first category, then Sandpolis probably doesn't offer much
value. For the rest of you, Sandpolis is an invaluable tool that can help shift
control of your virtual estate back where it belongs!

## How it works

Sandpolis is three applications:

- _the agent_ which is a headless daemon that runs on machines you want to
  control.
- _the client_ which is a GUI or CLI application that you use to interact with
  you virtual estate.
- _the server_ which ties everything together and stores persistent state.

If you don't want to run agents, you can also use _probes_ which offer similar
functionality, but reuse a common protocol like SSH instead.

Depending on how big your virtual estate is, you can also run multiple servers
in the same network arranged into _strata_: local stratum (LS) servers connect
upwards to a global stratum (GS) server. Here's an example network demonstrating
all of the above:

```
┌──────────────┐
│ Client (you) │
└──────┬───────┘
       │ mTLS
┌──────▼──────┐           ┌───────┐
│  GS Server  ◄───────────┤ Agent │
└──────▲──────┘   mTLS    └───────┘
       │ mTLS
┌──────┴──────┐           ┌───────────┐
│  LS Server  ┼───────────► SSH Probe │
└─────────────┘    SSH    └───────────┘
```

You connect your client to a server, which relays your requests to any agent or
probe in the estate. In the above example, the GUI client's shell layer opens a
terminal on either the agent or the SSH probe. From a terminal you address an
agent by its instance id:

```sh
sandpolis agents list                          # where the ids come from
sandpolis shell --instance agent-vo4xk2fqrzlm
```

All connections between instances (clients, servers, agents) are secured with
strict mTLS. This means you always need a certificate called a _realm
certificate_ to login. Servers can host multiple _realms_ which provide strong
isolation for different environments.

## Installation

<details>
<summary>Crates.io</summary>

![Crates.io Total Downloads](https://img.shields.io/crates/d/sandpolis)

#### Install from crates.io

One process is exactly one instance, and which instances a binary can be
started as is decided at build time. The default features turn on every
_subsystem_ but no instance, so pick at least one:

```sh
cargo install sandpolis --features client   # or server, or agent
```

Several can go in the same binary, which is what the demo image below does:

```sh
cargo install sandpolis --features server,agent,client
```

As an added benefit for this installation method, you can also drop the
subsystems you don't need. For example, a client with support for remote
desktop and nothing else:

```sh
cargo install sandpolis --no-default-features --features client,desktop
```

As a result, your installation artifacts will be smaller and will be unable to
perform any unwanted functionality.

</details>

<details>
<summary>Docker</summary>

#### Install server from DockerHub

![Docker Pulls](https://img.shields.io/docker/pulls/sandpolis/server)
![Docker Image Size](https://img.shields.io/docker/image-size/sandpolis/server)
![Docker Stars](https://img.shields.io/docker/stars/sandpolis/server)

```yml
# Docker compose
services:
  sandpolis-server:
    image: sandpolis/server
    restart: unless-stopped
```

#### Install client from DockerHub

![Docker Pulls](https://img.shields.io/docker/pulls/sandpolis/client)
![Docker Image Size](https://img.shields.io/docker/image-size/sandpolis/client)
![Docker Stars](https://img.shields.io/docker/stars/sandpolis/client)

```sh
alias sandpolis-client="docker run --rm sandpolis/client"
```

#### Try the whole thing at once

![Docker Image Size](https://img.shields.io/docker/image-size/sandpolis/demo)

The demo image runs a server, an agent and the GUI client together in one
container:

```sh
docker run --rm -it \
  -e XDG_RUNTIME_DIR \
  -e WAYLAND_DISPLAY \
  -v "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY":"$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" \
  --device /dev/dri \
  -v sandpolis-demo:/data \
  sandpolis/demo
```

Without a compositor handed in, the server and agent still come up and you get a
shell aimed at them instead of the GUI:

```console
[sandpolis demo] /data $ sandpolis agents list
```

</details>

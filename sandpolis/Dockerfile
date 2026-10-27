# syntax=docker/dockerfile:1.7
#
# Every Sandpolis image, built from one nix toolchain so a container links
# against exactly what `nix-shell shell.nix` gives the dev loop:
#
#   docker build --target server -f sandpolis/Dockerfile -t sandpolis/server .
#   docker build --target agent  -f sandpolis/Dockerfile -t sandpolis/agent  .
#   docker build --target client -f sandpolis/Dockerfile -t sandpolis/client .
#   docker build --target demo   -f sandpolis/Dockerfile -t sandpolis/demo   .
#
# The build context is the repository root, not this directory: `build.rs` walks
# every workspace member.

FROM nixos/nix:2.35.2 AS nix-base

# The nix sandbox wants user namespaces, which are unreliable under BuildKit, and
# the derivations here are all fetched or built the same either way.
RUN printf '%s\n' \
      'sandbox = false' \
      'filter-syscalls = false' \
      'max-jobs = auto' \
      'cores = 0' \
    >>/etc/nix/nix.conf

# Realize the toolchain before the source arrives, so editing code doesn't
# re-download several gigabytes of nixpkgs.
FROM nix-base AS deps
WORKDIR /build
COPY nix/ ./nix/
COPY shell.nix ./
RUN nix-shell shell.nix --run true

FROM deps AS build-server
COPY . .
ARG TARGETPLATFORM
RUN --mount=type=cache,id=sandpolis-registry-${TARGETPLATFORM},sharing=locked,target=/root/.cargo/registry \
    --mount=type=cache,id=sandpolis-git-${TARGETPLATFORM},sharing=locked,target=/root/.cargo/git \
    --mount=type=cache,id=sandpolis-target-server-${TARGETPLATFORM},sharing=locked,target=/build/target \
    nix-shell shell.nix --run "cargo build --release -p sandpolis --bin sandpolis --features server" \
    && install -Dm755 target/release/sandpolis /out/bin/sandpolis

FROM deps AS build-agent
COPY . .
ARG TARGETPLATFORM
RUN --mount=type=cache,id=sandpolis-registry-${TARGETPLATFORM},sharing=locked,target=/root/.cargo/registry \
    --mount=type=cache,id=sandpolis-git-${TARGETPLATFORM},sharing=locked,target=/root/.cargo/git \
    --mount=type=cache,id=sandpolis-target-agent-${TARGETPLATFORM},sharing=locked,target=/build/target \
    nix-shell shell.nix --run "cargo build --release -p sandpolis --bin sandpolis --features agent" \
    && install -Dm755 target/release/sandpolis /out/bin/sandpolis

FROM deps AS build-client
COPY . .
ARG TARGETPLATFORM
RUN --mount=type=cache,id=sandpolis-registry-${TARGETPLATFORM},sharing=locked,target=/root/.cargo/registry \
    --mount=type=cache,id=sandpolis-git-${TARGETPLATFORM},sharing=locked,target=/root/.cargo/git \
    --mount=type=cache,id=sandpolis-target-client-${TARGETPLATFORM},sharing=locked,target=/build/target \
    nix-shell shell.nix --run "cargo build --release -p sandpolis --bin sandpolis --features client" \
    && install -Dm755 target/release/sandpolis /out/bin/sandpolis

# One binary that can be started as any of the three.
FROM deps AS build-demo
COPY . .
ARG TARGETPLATFORM
RUN --mount=type=cache,id=sandpolis-registry-${TARGETPLATFORM},sharing=locked,target=/root/.cargo/registry \
    --mount=type=cache,id=sandpolis-git-${TARGETPLATFORM},sharing=locked,target=/root/.cargo/git \
    --mount=type=cache,id=sandpolis-target-demo-${TARGETPLATFORM},sharing=locked,target=/build/target \
    nix-shell shell.nix --run "cargo build --release -p sandpolis --bin sandpolis --features server,agent,client" \
    && install -Dm755 target/release/sandpolis /out/bin/sandpolis

# Kept separate from `deps` so that editing mkrootfs doesn't invalidate the
# toolchain layer and re-fetch nixpkgs.
FROM nix-base AS rootfs-base
COPY nix/ /work/nix/
COPY --chmod=0755 sandpolis/mkrootfs /usr/local/bin/mkrootfs
RUN sh -n /usr/local/bin/mkrootfs

FROM rootfs-base AS rootfs-server
COPY --from=build-server /out/bin/sandpolis /out/bin/sandpolis
RUN /usr/local/bin/mkrootfs server

FROM rootfs-base AS rootfs-agent
COPY --from=build-agent /out/bin/sandpolis /out/bin/sandpolis
RUN /usr/local/bin/mkrootfs agent

FROM rootfs-base AS rootfs-client
COPY --from=build-client /out/bin/sandpolis /out/bin/sandpolis
RUN /usr/local/bin/mkrootfs client

FROM rootfs-base AS rootfs-demo
COPY --from=build-demo /out/bin/sandpolis /out/bin/sandpolis
RUN /usr/local/bin/mkrootfs demo

COPY --chmod=0755 sandpolis/sandpolis-demo /rootfs/bin/sandpolis-demo
RUN /rootfs/runtime/bin/bash -n /rootfs/bin/sandpolis-demo

FROM scratch AS server
COPY --from=rootfs-server /rootfs/ /
ENV PATH=/bin:/runtime/bin \
    LD_LIBRARY_PATH=/runtime/lib \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt \
    LC_ALL=C \
    HOME=/tmp \
    S7S_DATA=/data
VOLUME /data
EXPOSE 8768/tcp 8768/udp
ENTRYPOINT [ "/bin/sandpolis" ]
CMD [ "server" ]

FROM scratch AS agent
COPY --from=rootfs-agent /rootfs/ /
ENV PATH=/bin:/runtime/bin \
    LD_LIBRARY_PATH=/runtime/lib \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt \
    LC_ALL=C \
    HOME=/tmp \
    S7S_DATA=/data \
    GST_PLUGIN_SYSTEM_PATH_1_0=/runtime/lib/gstreamer-1.0 \
    ALSA_CONFIG_PATH=/runtime/share/alsa/alsa.conf
VOLUME /data
ENTRYPOINT [ "/bin/sandpolis" ]
CMD [ "agent" ]

# The GUI reads more of the environment than the daemons do: the vulkan loader
# finds its drivers under $XDG_DATA_DIRS, and without XKB_CONFIG_ROOT the window
# comes up but takes no keyboard input.
FROM scratch AS client
COPY --from=rootfs-client /rootfs/ /
ENV PATH=/bin:/runtime/bin \
    LD_LIBRARY_PATH=/runtime/lib \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt \
    LC_ALL=C \
    HOME=/tmp \
    S7S_DATA=/data \
    XDG_DATA_DIRS=/runtime/share \
    XKB_CONFIG_ROOT=/runtime/share/X11/xkb \
    LIBGL_DRIVERS_PATH=/runtime/lib/dri \
    __EGL_VENDOR_LIBRARY_DIRS=/runtime/share/glvnd/egl_vendor.d \
    ALSA_CONFIG_PATH=/runtime/share/alsa/alsa.conf
VOLUME /data
ENTRYPOINT [ "/bin/sandpolis" ]
CMD [ "client" ]

# No S7S_DATA here: all three subcommands would read it and collide on one
# database. The entrypoint gives each instance its own directory under /data.
FROM scratch AS demo
COPY --from=rootfs-demo /rootfs/ /
ENV PATH=/bin:/runtime/bin \
    LD_LIBRARY_PATH=/runtime/lib \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt \
    LC_ALL=C \
    HOME=/tmp \
    XDG_DATA_DIRS=/runtime/share \
    XKB_CONFIG_ROOT=/runtime/share/X11/xkb \
    LIBGL_DRIVERS_PATH=/runtime/lib/dri \
    __EGL_VENDOR_LIBRARY_DIRS=/runtime/share/glvnd/egl_vendor.d \
    GST_PLUGIN_SYSTEM_PATH_1_0=/runtime/lib/gstreamer-1.0 \
    ALSA_CONFIG_PATH=/runtime/share/alsa/alsa.conf
VOLUME /data
EXPOSE 8768/tcp 8768/udp
ENTRYPOINT [ "/runtime/bin/bash", "/bin/sandpolis-demo" ]

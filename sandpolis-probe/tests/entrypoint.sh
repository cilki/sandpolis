#!/usr/bin/env bash
#
# Brings up one server per protocol the probe layer speaks, all in one
# container. Each protocol is a `start_<name>` function; `SERVICES` selects which
# of them run, so a developer working on one probe can start just that daemon.
#
# Every daemon's configuration is generated here rather than taken from the
# package: the nix store paths are read-only, so nothing may rely on a package's
# built-in sysconfdir.
set -euo pipefail

SERVICES="${SERVICES:-vnc rdp nfs smb rtsp ups ssh http snmp ipmi}"
VNC_PASSWORD="${VNC_PASSWORD:-password}"
GEOMETRY="${GEOMETRY:-1280x720}"
PROBE_USER="${PROBE_USER:-probe}"
PROBE_PASSWORD="${PROBE_PASSWORD:-password}"

# Which port variables the caller pinned. `check_ports` moves the rest when the
# container turns out not to be allowed to bind the standard ones.
PINNED_PORTS=""
for _var in SSH_PORT HTTP_PORT SNMP_PORT SMB_PORT RTSP_PORT IPMI_PORT; do
  [[ -n "${!_var:-}" ]] && PINNED_PORTS="${PINNED_PORTS} ${_var}"
done

# The defaults are the standard ports, which is what the probes look for. The
# mount service gets its own port because `unfs3` serves it separately from NFS;
# both are still registered with the portmapper, so resolving them via GETPORT
# works too.
SSH_PORT="${SSH_PORT:-22}"
HTTP_PORT="${HTTP_PORT:-80}"
SNMP_PORT="${SNMP_PORT:-161}"
SMB_PORT="${SMB_PORT:-445}"
RTSP_PORT="${RTSP_PORT:-554}"
IPMI_PORT="${IPMI_PORT:-623}"
NFS_PORT="${NFS_PORT:-2049}"
MOUNT_PORT="${MOUNT_PORT:-2050}"
RDP_PORT="${RDP_PORT:-3389}"
NUT_PORT="${NUT_PORT:-3493}"
VNC_PORT="${VNC_PORT:-5900}"

# The portmapper is pinned to 111 by the RPC protocol, so unlike everything else
# it has nowhere to move to.
PORTMAPPER=yes

# Where the ports below move when the standard ones are out of reach.
UNPRIVILEGED_PORTS="SSH_PORT=2222 HTTP_PORT=8080 SNMP_PORT=1161 SMB_PORT=4445 RTSP_PORT=8554 IPMI_PORT=1623"

# Where the Dockerfile realized the package set. Only the font path needs it;
# everything else is found on PATH.
TEST_PROBE_PREFIX="${TEST_PROBE_PREFIX:-/opt/test-probe}"

export DISPLAY=:0
export HOME="${HOME:-/root}"
export XAUTHORITY="${XAUTHORITY:-$HOME/.Xauthority}"
export NUT_CONFPATH=/etc/nut
export NUT_STATEPATH=/var/state/ups
# NUT otherwise repeats, for every process it starts, that it can't find the
# systemd it was built against.
export NUT_QUIET_INIT_UPSNOTIFY=true
export SERVICES GEOMETRY

log() {
  printf '[test-probe] %s\n' "$*" >&2
}

# The pid of every server, so the exit at the end can name what died. Call it
# directly after backgrounding one.
declare -A SERVICE_PID

track() {
  SERVICE_PID[$!]="$1"
}

# Whether `$1` appears in the SERVICES list.
wants() {
  [[ " ${SERVICES} " == *" $1 "* ]]
}

# Block until something is listening on a TCP port, so daemons that register
# with or publish to another daemon don't race it.
wait_for_port() {
  local port="$1"
  for _ in $(seq 1 100); do
    if (exec 3<>"/dev/tcp/127.0.0.1/${port}") 2>/dev/null; then
      return 0
    fi
    sleep 0.1
  done
  log "timed out waiting for port ${port}"
  return 1
}

# The tree both filesystem protocols serve. Its contents are asserted on by hand
# (see README.md), so keep them stable.
seed_tree() {
  local root="$1"
  mkdir -p "$root/docs/nested"
  printf 'top level' >"$root/top.txt"
  printf 'hello probe' >"$root/docs/readme.txt"
  : >"$root/docs/empty.txt"
  printf 'deep' >"$root/docs/nested/deep.txt"
}

# The account databases are symlinks into the read-only nix store in the base
# image, and `useradd`, `chpasswd`, and friends open them with O_NOFOLLOW: every
# account change fails with "cannot open /etc/passwd" until each link is replaced
# by a writable copy of whatever the image shipped.
unpin_etc_db() {
  local path="$1" fallback="$2" mode="$3"
  if [[ -f "$path" && ! -L "$path" ]]; then
    return
  fi
  local tmp="${path}.new"
  if [[ -e "$path" ]]; then
    cat "$path" >"$tmp"
  else
    printf '%b' "$fallback" >"$tmp"
  fi
  chmod "$mode" "$tmp"
  mv -f "$tmp" "$path"
}

# Everything the daemons expect under /etc that a `dockerTools` rootfs doesn't
# have. Runs once before any service starts.
seed_etc() {
  unpin_etc_db /etc/passwd 'root:x:0:0:root:/root:/bin/sh\n' 0644
  unpin_etc_db /etc/group 'root:x:0:\n' 0644
  unpin_etc_db /etc/shadow 'root:!::0:::::\n' 0600
  unpin_etc_db /etc/gshadow 'root:!::\n' 0600

  # `useradd` reads these from its own sysconfdir, which doesn't exist here, and
  # creates home directories under a /home that doesn't either.
  [[ -f /etc/login.defs ]] || : >/etc/login.defs
  mkdir -p /etc/default /home
  [[ -f /etc/default/useradd ]] || printf 'SHELL=/bin/sh\n' >/etc/default/useradd

  # The image ships no /etc/services, and several daemons resolve their port by
  # name -- rpcbind looks up `sunrpc` and exits if it can't, reporting only over
  # syslog. These are the ports this container serves.
  [[ -f /etc/services ]] || cat >/etc/services <<'EOF'
ssh	22/tcp
http	80/tcp		www
sunrpc	111/tcp		portmapper
sunrpc	111/udp		portmapper
snmp	161/udp
microsoft-ds	445/tcp
rtsp	554/tcp
rtsp	554/udp
asf-rmcp	623/udp
nfs	2049/tcp
nfs	2049/udp
ms-wbt-server	3389/tcp
nut	3493/tcp
x11	6000/tcp
EOF

  # Nothing tells glibc's NSS to read the files above without this.
  [[ -f /etc/nsswitch.conf ]] || cat >/etc/nsswitch.conf <<'EOF'
passwd: files
group: files
shadow: files
hosts: files dns
services: files
EOF

  # fontconfig has no system configuration in the image, so every Xft client --
  # xterm included -- dies with "Cannot load default config file" and the
  # desktop the probes connect to comes up empty.
  mkdir -p /etc/fonts /var/cache/fontconfig
  cat >/etc/fonts/fonts.conf <<EOF
<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
<fontconfig>
  <dir>${TEST_PROBE_PREFIX}/share/fonts</dir>
  <cachedir>/var/cache/fontconfig</cachedir>
</fontconfig>
EOF
}

# Whether this container may bind a port below 1024 at all. 1023 stands in for
# the real ports, which may be legitimately busy on a shared network namespace.
can_bind_privileged() {
  nc -l -p 1023 >/dev/null 2>&1 &
  local pid=$!
  sleep 0.5
  local bound=0
  if kill -0 "$pid" 2>/dev/null; then
    bound=1
    kill "$pid" 2>/dev/null || true
  fi
  # Reap it either way: an unwaited-for job is one the `wait -n` at the end
  # would take for a server that died.
  wait "$pid" 2>/dev/null || true
  ((bound))
}

# Binding a port below 1024 needs CAP_NET_BIND_SERVICE, which a rootless Docker
# or Podman container doesn't have when it shares the host's network namespace
# (`--network host`): every daemon on a standard port fails, some of them
# quietly. Move them rather than serve nothing.
check_ports() {
  if can_bind_privileged; then
    return
  fi

  log "this container may not bind ports below 1024 (no CAP_NET_BIND_SERVICE)."
  log "rootless Docker and Podman lose that when they share the host's network;"
  log "publishing ports instead of --network host keeps the standard numbers."

  local assignment var value
  for assignment in $UNPRIVILEGED_PORTS; do
    var="${assignment%%=*}"
    value="${assignment#*=}"
    if [[ " ${PINNED_PORTS} " == *" ${var} "* ]]; then
      continue
    fi
    printf -v "$var" '%s' "$value"
    log "moved ${var%_PORT} to ${value}"
  done

  PORTMAPPER=no
  log "the portmapper is fixed at 111, so it stays off; NFS still serves ${NFS_PORT}"
}

# A container has no /dev/log, and several daemons report even fatal errors only
# over syslog -- `rpcbind` opens it without LOG_CONS or LOG_PERROR, so a failed
# startup prints nothing at all. Relay the socket to stdout before anything else
# runs, and those messages land in `docker logs` with the rest.
start_syslog() {
  rm -f /dev/log
  syslogd -n -O - &
  disown
  for _ in $(seq 1 50); do
    [[ -S /dev/log ]] && break
    sleep 0.1
  done
  [[ -S /dev/log ]] || log "no /dev/log; daemons that log only over syslog will be silent"
}

# The unix account SMB and SSH authenticate against. NUT and IPMI reuse the same
# name and password, but only inside their own config files.
ensure_user() {
  if id "$PROBE_USER" >/dev/null 2>&1; then
    return
  fi
  # `-U` gives the account a group of its own; without it `useradd` falls back
  # to a default GID that no group in this image claims.
  useradd --create-home --user-group --shell /bin/sh "$PROBE_USER"
  # `chpasswd` authenticates through PAM unless a crypt method is named, and the
  # image has no PAM configuration at all ("pam_start failure 26"). `-c` makes it
  # write the hash to /etc/shadow itself, which is what sshd (UsePAM no) reads.
  printf '%s:%s\n' "$PROBE_USER" "$PROBE_PASSWORD" | chpasswd -c SHA512
}

# Xvnc is both the X server for :0 and its VNC server, so an RDP-only run still
# starts it (and still listens on VNC_PORT). It is a virtual framebuffer:
# the desktop the probes see exists only in this container, and nothing here
# reads or needs a display on the host.
start_x() {
  log "starting X/VNC on ${VNC_PORT}"

  # Xvnc reads its password from an obfuscated file; `vncpasswd -f` produces one
  # from the plaintext on stdin.
  mkdir -p "$HOME/.vnc"
  vncpasswd -f <<<"$VNC_PASSWORD" >"$HOME/.vnc/passwd"
  chmod 600 "$HOME/.vnc/passwd"

  # X clients need a cookie to reach the display. Without an auth file the
  # server has no way to admit them, and the desktop stays empty.
  : >"$XAUTHORITY"
  xauth -f "$XAUTHORITY" add "$DISPLAY" MIT-MAGIC-COOKIE-1 \
    "$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"

  # `-nolisten tcp` keeps the X protocol off port 6000, which matters under
  # `--network host`: the host may already have a server bound there, and the
  # container's clients must reach this display over its own unix socket.
  Xvnc :0 \
    -geometry "$GEOMETRY" \
    -depth 24 \
    -rfbport "$VNC_PORT" \
    -SecurityTypes VncAuth \
    -PasswordFile "$HOME/.vnc/passwd" \
    -AlwaysShared \
    -auth "$XAUTHORITY" \
    -nolisten tcp &
  track Xvnc

  # Wait for the display to accept connections before starting clients on it.
  for _ in $(seq 1 50); do
    xdpyinfo >/dev/null 2>&1 && break
    sleep 0.2
  done
  xdpyinfo >/dev/null 2>&1 ||
    log "X did not come up; VNC and RDP will serve a blank desktop"

  # Something has to draw on the virtual display, or both the VNC and the RDP
  # probe get a bare root window. The clock ticks so a capture is visibly live.
  # These aren't servers: `disown` keeps a client exiting from taking the
  # container down with it (see the `wait -n` at the end).
  xsetroot -solid '#1f3b57' || true
  openbox &
  disown
  xterm -title 'sandpolis test probe' -fa 'DejaVu Sans Mono' -fs 12 \
    -geometry 100x30+40+40 -e bash -c '
      printf "sandpolis test probe\n\n"
      printf "display:  %s (%s)\n" "$DISPLAY" "$GEOMETRY"
      printf "services: %s\n\n" "$SERVICES"
      while :; do
        printf "\r%s" "$(date -u "+%Y-%m-%d %H:%M:%SZ")"
        sleep 1
      done' &
  disown
}

# freerdp-shadow-cli mirrors the same :0 over RDP. It generates a self-signed
# certificate on first run, which the probe accepts (it negotiates TLS security,
# not NLA). Adjust the flags here if your freerdp build differs.
start_rdp() {
  log "starting RDP on ${RDP_PORT}"
  freerdp-shadow-cli /port:"$RDP_PORT" &
  track freerdp-shadow-cli
}

start_nfs() {
  if [[ "$PORTMAPPER" == yes ]]; then
    log "starting NFS on ${NFS_PORT} (mount ${MOUNT_PORT}, portmapper 111)"
  else
    log "starting NFS on ${NFS_PORT} (mount ${MOUNT_PORT}, no portmapper)"
  fi
  seed_tree /srv/nfs

  # A bare option list exports to every host; `unfsd` rejects a `*` wildcard.
  # `insecure` accepts clients whose source port is unprivileged, so the probe
  # works with `privileged_port` either way.
  cat >/etc/exports <<'EOF'
/srv/nfs (rw,no_root_squash,insecure)
EOF

  # Without a portmapper the probe can still reach the pinned ports, so a failed
  # rpcbind degrades the GETPORT path rather than the whole container.
  if [[ "$PORTMAPPER" == yes ]] && start_rpcbind && wait_for_port 111; then
    unfsd -d -e /etc/exports -n "$NFS_PORT" -m "$MOUNT_PORT" &
    track unfsd
    return
  fi

  if [[ "$PORTMAPPER" == yes ]]; then
    log "rpcbind did not come up; serving NFS without portmapper registration"
  fi
  unfsd -d -p -e /etc/exports -n "$NFS_PORT" -m "$MOUNT_PORT" &
  track unfsd
}

start_rpcbind() {
  # rpcbind is built `--with-rpcuser=rpc`: it drops privileges to that account at
  # startup and exits if it can't resolve it. The `sunrpc` service entry it also
  # insists on is written by `seed_etc`.
  if ! id rpc >/dev/null 2>&1; then
    useradd -r -U -d /var/lib/rpcbind -s /bin/sh rpc || true
  fi
  id rpc >/dev/null 2>&1 ||
    log "the rpc user is missing; rpcbind will exit at startup"

  # rpcbind chowns its state directory only when it creates the directory
  # itself, so a pre-made one leaves the warm-start files unwritable once it has
  # dropped privileges.
  mkdir -p /run/rpcbind /var/lib/rpcbind
  chown rpc /run/rpcbind /var/lib/rpcbind 2>/dev/null || true
  rpcbind -f -w &
  track rpcbind
}

start_smb() {
  log "starting SMB on ${SMB_PORT}"
  ensure_user
  seed_tree /srv/smb
  chown -R "$PROBE_USER" /srv/smb
  # `ncalrpc dir` and the core path are the two directories smbd creates for
  # itself, under /var/run and /var/log, neither of which this image has.
  mkdir -p /var/lib/samba/{private,lock,state,cache} /run/samba/ncalrpc /var/log/cores

  cat >/etc/smb.conf <<EOF
[global]
  workgroup = WORKGROUP
  server string = sandpolis test probe
  security = user
  smb ports = $SMB_PORT
  # No nmbd, so nothing needs the NetBIOS ports.
  disable netbios = yes
  server min protocol = SMB2
  map to guest = Never
  # Everything Samba writes has to land outside the read-only nix store.
  private dir = /var/lib/samba/private
  lock directory = /var/lib/samba/lock
  state directory = /var/lib/samba/state
  cache directory = /var/lib/samba/cache
  pid directory = /run/samba
  ncalrpc dir = /run/samba/ncalrpc
  logging = file
  log file = /var/log/samba.log

[media]
  path = /srv/smb
  browseable = yes
  writable = yes
  valid users = $PROBE_USER
EOF

  # The `smb` client crate authenticates with NTLMv2 inside SPNEGO (Kerberos is
  # off), which Samba's default accepts.
  printf '%s\n%s\n' "$PROBE_PASSWORD" "$PROBE_PASSWORD" |
    smbpasswd -s -c /etc/smb.conf -a "$PROBE_USER"

  smbd --foreground --no-process-group --debug-stdout -s /etc/smb.conf &
  track smbd
}

start_rtsp() {
  log "starting RTSP on ${RTSP_PORT}"

  # ffmpeg publishes a test pattern into mediamtx, which restreams it. Baseline
  # H.264 in yuv420p because the client decodes with openh264.
  cat >/etc/mediamtx.yml <<EOF
logLevel: info
rtspAddress: :${RTSP_PORT}
# Nothing here restreams over anything but RTSP.
rtmp: no
hls: no
webrtc: no
srt: no
api: no
metrics: no
pprof: no
paths:
  stream1:
    runOnInit: >
      ffmpeg -nostats -loglevel warning -re -f lavfi -i testsrc=size=1280x720:rate=30
      -pix_fmt yuv420p -c:v libx264 -profile:v baseline -preset ultrafast
      -tune zerolatency -g 30 -f rtsp rtsp://127.0.0.1:${RTSP_PORT}/stream1
    runOnInitRestart: yes
EOF

  mediamtx /etc/mediamtx.yml &
  track mediamtx
}

start_ups() {
  log "starting NUT on ${NUT_PORT}"
  mkdir -p "$NUT_CONFPATH" "$NUT_STATEPATH"
  # upsd warns about a world-readable state directory on every start.
  chmod 0700 "$NUT_STATEPATH"

  printf 'MODE=standalone\n' >"$NUT_CONFPATH/nut.conf"

  cat >"$NUT_CONFPATH/ups.conf" <<'EOF'
[test]
  driver = dummy-ups
  port = test.dev
  desc = "sandpolis test UPS"
EOF

  # dummy-ups replays this file in a loop, so the probe sees live variables.
  cat >"$NUT_CONFPATH/test.dev" <<'EOF'
device.mfr: Sandpolis
device.model: Test UPS
device.type: ups
ups.mfr: Sandpolis
ups.model: Test UPS
ups.status: OL
ups.load: 23
battery.charge: 100
battery.runtime: 3600
input.voltage: 120.0
output.voltage: 120.0
EOF

  cat >"$NUT_CONFPATH/upsd.conf" <<EOF
LISTEN 0.0.0.0 $NUT_PORT
EOF

  cat >"$NUT_CONFPATH/upsd.users" <<EOF
[$PROBE_USER]
  password = $PROBE_PASSWORD
  upsmon primary
  actions = SET
  instcmds = ALL
EOF
  chmod 600 "$NUT_CONFPATH/upsd.users" "$NUT_CONFPATH/upsd.conf"

  upsdrvctl -u root start
  upsd -u root -D &
  track upsd
}

start_ssh() {
  log "starting SSH on ${SSH_PORT}"
  ensure_user
  mkdir -p /etc/ssh /var/empty
  chmod 755 /var/empty

  # Privilege separation is mandatory and wants a dedicated account.
  if ! id sshd >/dev/null 2>&1; then
    useradd -r -U -d /var/empty -s /bin/sh sshd
  fi

  [[ -f /etc/ssh/ssh_host_ed25519_key ]] ||
    ssh-keygen -q -t ed25519 -N '' -f /etc/ssh/ssh_host_ed25519_key
  [[ -f /etc/ssh/ssh_host_rsa_key ]] ||
    ssh-keygen -q -t rsa -b 3072 -N '' -f /etc/ssh/ssh_host_rsa_key

  cat >/etc/ssh/sshd_config <<EOF
Port $SSH_PORT
ListenAddress 0.0.0.0
HostKey /etc/ssh/ssh_host_ed25519_key
HostKey /etc/ssh/ssh_host_rsa_key
PasswordAuthentication yes
PermitRootLogin yes
UsePAM no
StrictModes no
PrintMotd no
PidFile /run/sshd.pid
EOF

  # sshd re-execs itself and refuses to start unless invoked by absolute path.
  "$(command -v sshd)" -D -e -f /etc/ssh/sshd_config &
  track sshd
}

start_http() {
  log "starting HTTP on ${HTTP_PORT}"
  mkdir -p /srv/http

  cat >/srv/http/index.html <<'EOF'
<!doctype html>
<title>sandpolis test probe</title>
<h1>sandpolis test probe</h1>
EOF

  cat >/etc/lighttpd.conf <<EOF
server.document-root = "/srv/http"
server.port = $HTTP_PORT
server.modules = ( "mod_dirlisting" )
dir-listing.activate = "enable"
index-file.names = ( "index.html" )
mimetype.assign = ( ".html" => "text/html", ".txt" => "text/plain" )
EOF

  lighttpd -D -f /etc/lighttpd.conf &
  track lighttpd
}

start_snmp() {
  log "starting SNMP on ${SNMP_PORT}/udp"
  mkdir -p /var/lib/net-snmp

  cat >/etc/snmpd.conf <<'EOF'
rocommunity public
sysLocation sandpolis test container
sysContact probe@example.com
sysServices 72
EOF

  snmpd -f -Lo -C -c /etc/snmpd.conf "udp:${SNMP_PORT}" &
  track snmpd
}

start_ipmi() {
  log "starting IPMI on ${IPMI_PORT}/udp"
  mkdir -p /etc/ipmi /var/ipmi_sim

  cat >/etc/ipmi/lan.conf <<EOF
name "ipmisim1"

set_working_mc 0x20

  startlan 1
    addr 0.0.0.0 $IPMI_PORT
    priv_limit admin
    allowed_auths_callback none md2 md5 straight
    allowed_auths_user none md2 md5 straight
    allowed_auths_operator none md2 md5 straight
    allowed_auths_admin none md2 md5 straight
    guid a123456789abcdefa123456789abcdef
  endlan

  startnow false

  user 1 true  ""            "test"             user  10 none md2 md5 straight
  user 2 true  "$PROBE_USER" "$PROBE_PASSWORD"  admin 10 none md2 md5 straight
EOF

  cat >/etc/ipmi/sim.emu <<'EOF'
mc_setbmc 0x20
mc_add 0x20 0 no-device-sdrs 0x23 9 8 0x9f 0x1291 0xf02
sel_enable 0x20 1000 0x0a
mc_enable 0x20
EOF

  # -n: no stdio console. -p: don't persist simulated state.
  ipmi_sim -c /etc/ipmi/lan.conf -f /etc/ipmi/sim.emu -n -p -s /var/ipmi_sim &
  track ipmi_sim
}

if [[ -z "${SERVICES// /}" ]]; then
  log "SERVICES is empty; nothing to run"
  exit 1
fi

for service in $SERVICES; do
  case "$service" in
  vnc | rdp | nfs | smb | rtsp | ups | ssh | http | snmp | ipmi) ;;
  *)
    log "unknown service: ${service}"
    exit 1
    ;;
  esac
done

seed_etc
start_syslog
check_ports

if wants vnc || wants rdp; then
  start_x
fi
if wants rdp; then start_rdp; fi
if wants nfs; then start_nfs; fi
if wants smb; then start_smb; fi
if wants rtsp; then start_rtsp; fi
if wants ups; then start_ups; fi
if wants ssh; then start_ssh; fi
if wants http; then start_http; fi
if wants snmp; then start_snmp; fi
if wants ipmi; then start_ipmi; fi

log "services up: ${SERVICES}"

# What actually came up, which is the answer to most "why can't the probe reach
# it" questions -- not least after `check_ports` has moved something. The last
# daemons started need a moment to bind before the summary means anything.
sleep 2
ss -lntup 2>/dev/null | sed 's/^/[test-probe] /' >&2 || true

# Exit (and let the container stop) as soon as any server dies.
dead_pid=""
dead_status=0
wait -n -p dead_pid || dead_status=$?
log "${SERVICE_PID[$dead_pid]:-a service} exited (status ${dead_status}); stopping"
exit "$dead_status"

# fail2ban integration

The Sandpolis server logs a canonical line at `WARN` level whenever an
authentication attempt fails — a bad password or TOTP code, an unknown user, a
missing/invalid connection token, or a rejected client certificate:

```
2026-09-02T16:00:00.000000Z  WARN sandpolis_server::login::server: Authentication failure peer=203.0.113.7 username=admin
```

fail2ban can watch for these lines and ban the offending address at the
firewall, which replaces any in-application IP blocklist. The example jail bans
on every port (`iptables-allports`) because the server listens on its one port
number over both TCP (HTTPS, where logins happen) and UDP (WebTransport, where
instances attach); a per-port ban would cover only one of the two.

## Installation

```sh
cp filter.d/sandpolis.conf /etc/fail2ban/filter.d/
cp jail.d/sandpolis.conf /etc/fail2ban/jail.d/
# edit /etc/fail2ban/jail.d/sandpolis.conf: set enabled = true
systemctl reload fail2ban
```

The example jail assumes the server runs under systemd as `sandpolis.service`,
so its stderr lands in the journal and fail2ban's `systemd` backend can read it
directly. If the unit is named differently, adjust `journalmatch` in the
filter. Outside systemd, capture the server's stderr to a file and point
`logpath` at it (see the comments in the jail file).

## Testing the filter

With the server running, make a failing login attempt, then:

```sh
fail2ban-regex systemd-journal /etc/fail2ban/filter.d/sandpolis.conf
```

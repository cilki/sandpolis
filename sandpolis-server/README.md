## `sandpolis-server`

This subsystem implements server-related functionality: the strata that servers
arrange themselves into, the ownership of data between them, the banner a server
presents to instances that connect to it, and the user accounts clients login
to.

## Users

User accounts come from the `user` section of a realm config, which is the only
place they are ever created, changed, or removed. The global stratum server
reconciles that list into the realm database on every start: a username the
config gained is created, one it lost is deleted along with its password hash
and TOTP secret. Nothing creates a user at runtime.

```ron
(
  user: (
    // Require a TOTP secret, enrolled when a user sets their password.
    totp: false,
    users: [
      // `"layer:action"`, or `"shell:*"` for a whole layer, or `"*"` for
      // everything. There is no separate admin flag.
      (username: ("admin"), permissions: ["*"]),
      (username: ("oncall"), permissions: ["shell:*", "filesystem:session"]),
    ],
  ),
)
```

A realm whose config declares no users is **open**: the realm certificate is
the only credential, and clients connect without logging in. Agents never login
either way — their certificate is what authorizes them.

The first login under a configured username sets that account's password. It is
first-come-first-served, gated only on possession of the realm cert and the
username, so a realm that matters should have its users log in once before the
cert travels any further. With `totp: true` the same exchange hands back an
otpauth URL to enroll, which the user then proves with a code.

## User sessions

A successful login returns a session token (JWT) that the client presents as a
bearer token on every subsequent request. Its lifetime is the realm's
`token_lifetime`, 30 days when the config doesn't set one; a client may request
less and anything longer is clamped to the realm's maximum. There is no
renewal — a client caches the token and logs in again once it expires.

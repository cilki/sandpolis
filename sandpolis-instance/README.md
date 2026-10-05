## `sandpolis-instance`

This subsystem implements what every instance has regardless of which one it is:
instance ids, realms, the connections and streams between instances, and the
data model the other subsystems are built on.

The data model is fundamental to all other subsystems. All instances maintain
their own database for different reasons:

- The server's database persists data for the entire network for long periods of
  time
- The client's database caches data fetched from the server temporarily while
  the user interacts with the application
- The agent's database caches data before it's sent to the server

### `Data` objects

Entries in the database (uncreatively called `Data`) are defined by Rust
structs:

```rs
#[data]
pub struct ExampleData {
    pub value: u32,
}
```

In the database, `Data` are stored as key-value pairs.

#### Resident `Data`

Certain `Data` may be brought into memory for ease of use and faster access.
There are two types to simplify this:

- `Resident`
- `ResidentVec`

#### `Data` ownership

## Realms

Sandpolis networks are partitioned into _realms_ which provide strong data
separation. Server and client instances can participate in multiple realms
simultaneously while agent instances belong to one realm at a time.

As an example, you can have _work_ and _home_ realms that are completely
isolated (other than running on the same server).

A realm exists because a realm config declares it — a `<realm>.realm.ron`
file, named for the realm — and the global stratum server serves every such
file in its `--data` directory. Nothing creates a realm at runtime.

### Realm membership

A realm's users are declared in its realm config, along with the permissions
each one gets, so a user account belongs to exactly one realm and is never
created any other way. See `sandpolis-server` for the details.

### Default realm

`default` is the realm name used wherever nothing names one: a server whose
`--data` directory holds no realm config gets a blank `default.realm.ron`
written for it, and a server started without `--data` serves an implicit
`default` realm out of its in-memory database. It is otherwise an ordinary
realm — a server whose directory declares only `work.realm.ron` and
`home.realm.ron` serves no realm called `default`.

### Realm authentication

All connections to a server instance must be authenticated with a TLS
certificate for a particular realm. A client or agent certificate encodes the
server's address in its common name (`host:port/realm`), so the certificate
names exactly one server and realm. It is distributed in a realm cert
(`<realm>.realm.pem`) alongside the realm CA that verifies the server.

There are three types of certificate:

#### Realm cluster certificate

Each realm has a single "root" cluster certificate that signs every other
certificate in the realm. Only the global stratum server holds its private key,
so it is the only instance that can issue — a local stratum server can verify
peers but never mint a certificate of its own.

#### Realm server certificate

A server's listener identity, used by clients and agents to verify the server is
part of the cluster.

#### Realm endpoint certificate

A clientAuth certificate for anything that dials a server: clients, agents, and
a local stratum server authenticating to its global stratum server all hold the
same kind. The server verifies it was issued by the cluster certificate and does
not distinguish a client from an agent by it — what a client is allowed to do
comes from the user it logs in as (when the realm declares users at all), not
from the certificate that got it onto the network.

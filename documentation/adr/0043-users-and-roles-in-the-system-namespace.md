# ADR 0043: Users and roles live in a reserved system namespace

Status: accepted
Date: 2026-10-04

## Context

Step 15a adds users with passwords, roles per namespace, a server-wide admin role and API tokens. They are durable server state: they must survive restarts, be backed up and restored, and change all or nothing. The design rules constrain where they can live:

- Rule 2: every change goes through one commit pipeline and the WAL; no second write path.
- Rule 3: a crash test proves that a user or grant change is all-or-nothing and survives recovery.
- Rule 4: any new on-disk format has magic bytes, a version and checksums, a reader for N-1 and a fixture.

Three places were considered:

1. **A namespace's catalog** (ADR 0003). The catalog is per namespace, in the namespace's checkpoint and WAL. Users are server-wide (an admin, a user with grants on several namespaces), so they don't fit a namespace's catalog, and a store of many namespaces has no single catalog to hold them.
2. **The namespace log** (`NAMESPACES`, ADR 0017). Store-level, framed and checksummed, fsynced per event. But it records namespace events only; adding users means new event kinds, a format bump, and a second kind of state replayed from it. It has no checkpoints, so every password change would stay in it for good.
3. **A store-level log of its own**: a new file with its own frames, fsync, torn-tail handling, recovery, backup, archive and restore code, its own crash tests and fixtures. That is a second commit pipeline in all but name, which rule 2 forbids.
4. **A reserved namespace**, whose graph holds the users, written through the store's existing commit pipeline.

## Decision

**Users, grants and API tokens are nodes of a reserved namespace, `_system`**, written with ordinary commits.

- **The name.** `NamespaceName` accepts exactly one name that starts with `_`: `_system` (`NamespaceName::SYSTEM`). No name a user picks can be it. The store refuses it to every public call: `create_namespace`, `drop_namespace`, `import_namespace` (`invalid_argument`, "reserved for the store's users and grants"), and `namespace()` answers `NoSuchNamespace`, so the `Database` trait, Python and `iwctl` can't reach it. `namespaces()` and `status()` leave it out. `verify`, `backup`, `restore`, checkpoints and the background threads see it like any other namespace, which is the point.
- **Created with the first user**, under the namespace log's mutex like any create. A store without users has no `_system` and is byte for byte what it was before step 15a, so older binaries keep opening it. Once a store has users, a binary from before step 15a refuses to open it (its namespace log names `_system`, which the older `NamespaceName` rejects: `InvalidNamespaceLog`). It refuses with an error and never misreads, which is what a layout bump would achieve. The layout version stays 5 because the encoding of every file is unchanged. Downgrading a store with users isn't supported (stated in data-dir.md).
- **Records** (`documentation/formats/auth.md`). Node `auth` with `{format: 1}` (the records' version, checked on every read: another format is `corrupt` and refused); `user:<name>` with `name`, `password` (an argon2id PHC string, ADR 0044), `admin`, `epoch` (grows with every password change; sessions of an older epoch end), `grants` (a dict from namespace id to `read`, `write` or `admin`) and `created`; `token:<sha256 hex>` with `user`, `name`, `created` and an optional `expires`. Magic bytes, checksums and torn tails are the WAL's and the checkpoints', which hold these records like any other data.
- **One commit per change.** Creating a user, a password change, a grant, deleting a user with its tokens: each is one commit to `_system`, all or nothing, durable per the store's fsync policy when it returns, replayed by recovery. Changes are serialized by a mutex of the store (each reads the records, then commits what it read plus the change), so no update is lost between two concurrent grants.
- **Grants name namespaces by id**, not by name. A namespace dropped and created again under its name is a new namespace (ADR 0017) and starts without grants; a grant needs the namespace to exist. A principal's grants are turned into names when it is authenticated.
- **Interfaces.** `Store::users()` (`iwdb::auth::Users`) is the synchronous API, used offline by `iwctl user` and by the bootstrap; `Embedded` implements the `Accounts` trait on it for the servers (ADR 0045).
- **Backups and restores carry the users**, at the backup's or the restore's point. A restore to an older time brings back the users and passwords of that time; a restore limited to some namespaces (`-n`) leaves `_system` out unless it is named.

## Consequences

- No new file format, no new recovery path: the WAL, checkpoints, the crash harness, backup, archive, restore and verify cover the users as they are. The crash test (`tests/crash/tests/auth_points.rs`) kills a child at every WAL write (before, halfway, after) and fsync of a scripted sequence of 17 changes and at the system namespace's creation, under `always` and `group`, and finds the users of the acknowledged changes or of one more, never part of one.
- Namespace ids now skip the one `_system` took: a store with users gives its next namespace id 3, not 2. Ids only ever promised to grow and never be reused.
- A store with users can't be opened by a binary from before step 15a (above). The fixture `crates/iwdb/tests/fixtures/auth-v1/` holds auth format 1 in a checkpoint and in WAL records; every later version must read it.
- Users are few; listing them reads every node of `_system` (O(users + tokens)). Authenticating a request reads two nodes under the namespace's read lock.

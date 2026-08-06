# Remotes and refspecs

Named remotes are configuration, not transports. `Remote` exposes ordered,
byte-preserving fetch URLs and push URLs plus validated `RefSpec` values. The
application remains responsible for selecting an HTTP, SSH, in-process, or
custom `UploadPackTransport`/`ReceivePackTransport` for a URL; the library never
opens a host process.

```rust
let remote = repository.add_remote("origin", b"memory://server")?;
assert_eq!(
    remote.fetch_refspecs()[0].to_string(),
    "+refs/heads/*:refs/remotes/origin/*"
);
repository.add_remote_fetch_refspec("origin", "^refs/heads/private/*")?;
let result = repository.fetch_remote("origin", &mut transport, &options)?;
# Ok::<(), git_rs::Error>(())
```

`add_remote`, `remote`, and `remotes` create and inspect configuration.
`set_remote_urls` preserves multiple URL values and distinguishes fetch URL
from push URL lists. Fetch and push refspecs can be appended only after parsing
successfully. Config changes use the same lock-and-rename storage path on host,
memory, and custom filesystems.

`rename_remote` migrates the standard `refs/remotes/<old>/` namespace,
symbolic remote `HEAD`, reflogs, fetch destinations, branch `remote` and
`pushRemote` settings, and `remote.pushDefault`. Direct tracking refs move in a
single compare-and-swap reference transaction.

`remove_remote` removes the remote's own tracking namespace and clears branch
upstream settings. A custom fetch destination outside
`refs/remotes/<remote>/` is returned in `RemoveRemoteResult::retained_refs`
rather than being silently deleted, because it may be a shared local namespace.

## Refspecs

`RefSpec::parse_fetch` and `parse_push` distinguish direction-specific rules:

- leading `+` force and fetch-only `^` exclusion;
- exact and corresponding single-wildcard mappings;
- omitted and empty fetch destinations;
- push deletion (`:refs/heads/name`) and matching branches (`:`);
- exact fetch object IDs and `@` normalization to `HEAD`.

`matches`, `map_destination`, and `matches_destination` operate without regex
allocation. Named fetch selects advertised refs through all positive rules,
then removes negative matches. It supports multiple destinations and applies
all resulting ref edits transactionally after the incoming pack is validated
and published. Non-forced commit updates require bounded fast-forward ancestry;
other non-forced rewrites are rejected.

## Source correspondence

Parsing was compared with `refspec.c:parse_refspec` and wildcard mapping with
`match_refname_with_pattern`. Remote mutation behavior was compared with
`builtin/remote.c` add, rename, and remove paths. This is independently written
safe Rust over git-rs configuration, reference, pack, and filesystem APIs.

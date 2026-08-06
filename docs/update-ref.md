# Transactional reference updates

`Repository::update_refs` applies typed direct and symbolic reference commands
through one prepared transaction. Commands include update, create, delete, and
verify plus their symbolic equivalents.

~~~rust
use git_rs::{UpdateRefCommand, UpdateRefOptions};

repository.update_refs(
    &[
        UpdateRefCommand::Update {
            name: "HEAD".into(),
            new: new_commit,
            old: Some(old_commit),
            no_deref: false,
        },
        UpdateRefCommand::Create {
            name: "refs/tags/checkpoint".into(),
            new: new_commit,
            no_deref: false,
        },
    ],
    &UpdateRefOptions::default(),
)?;
# Ok::<(), git_rs::Error>(())
~~~

Direct commands follow symbolic references by default, so updating `HEAD`
updates its branch. `no_deref` targets the named ref itself. Symbolic commands
always operate on the symbolic ref and can compare either its exact old target
or the object ID it resolves to. Create requires absence, delete requires an
existing ref, and verify changes no bytes. New direct targets must exist;
branch targets must be commit objects.

All destinations are validated and sorted before mutation. Their `.lock` files
are acquired before any old-value comparison. Packed deletions are prepared
under `packed-refs.lock`; stale direct or symbolic values, duplicate names, or
lock failures clean every prepared lock and publish nothing. The command count
and object validation reads are bounded.

Single-reference APIs `update_reference_with_reflog` and
`update_symbolic_reference` provide atomic reflog creation when a reason and
explicit `Signature` are required. Keeping identity explicit avoids ambient
process state and works with memory or remote filesystem adapters.

The implementation follows `builtin/update-ref.c`, `refs.c`, and the files
reference transaction backend. Tests cover dereferenced `HEAD`, mixed direct
and symbolic transactions, exact CAS conditions, missing objects, stale
rollback, root pseudorefs, lock cleanup, and packed deletion. Host checks
compare resulting direct and symbolic ref bytes with native Git.

~~~text
cargo run --example update_ref -- REPOSITORY update HEAD NEW OLD
cargo run --example update_ref -- REPOSITORY symref-create refs/meta/current refs/heads/main
~~~

# Architecture

Every operation is expressed in terms of `FileSystem`, never `std::fs`. A
`Repository` owns an `Arc<dyn FileSystem>` and repository-relative paths. This
lets an application route refs to a transactional database and objects to object
storage without changing Git logic.

The host adapter maps repository-relative paths beneath a configured root. Its
output is ordinary Git data: a non-bare repository has `.git/HEAD`, `.git/config`,
`objects`, and `refs`; a bare repository places those entries at its root.

Mutations that become visible to concurrent readers exclusively create the
canonical `.lock` file, write its complete contents, and atomically rename it.
Future ref and packed-ref transactions will build on this same primitive.

## Reference implementation comparisons

Behavior is compared against the Git source checkout, not copied from it:

- repository layout and templates: `builtin/init-db.c` and `setup.c`
- ref-name restrictions: `refs.c` (`check_refname_format`)
- lock-and-rename publication: `lockfile.c` and `refs/files-backend.c`

Tests state the behavior being compared so changes remain reviewable.

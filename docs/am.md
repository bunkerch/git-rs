# Applying email patch series

`Repository::am` parses inline RFC 2822 patch messages, applies each patch to
the abstract worktree and index, creates commits with the mail author, and uses
an explicit caller-supplied committer. It accepts messages returned by
`Repository::format_patches` and ordinary inline `git format-patch` output.

```rust
use git_rs::{AmOptions, Repository, Signature};

fn apply_series(
    repository: &Repository,
    mails: &[Vec<u8>],
) -> git_rs::Result<Vec<git_rs::ObjectId>> {
    let committer = Signature::new("Receiver", "receiver@example.com", 1_700_000_000, 0)?;
    Ok(repository
        .am(mails, &committer, &AmOptions::default())?
        .commits)
}
```

The parser unfolds headers, decodes UTF-8 RFC 2047 Base64 and Q encoded words,
parses quoted display names and numeric RFC 2822 timezone offsets, removes the
`[PATCH ...]` prefix from commit subjects, separates the log message at the
`---` marker, and passes the inline `diff --git` payload to the unified-patch
engine. Authors and their timestamps are preserved; committers are never read
from the environment or host clock.

## Recovery

Before starting, tracked worktree and index state must be clean. Untracked files
are allowed and remain protected by patch collision checks. The complete series,
original tip, original HEAD mode, and current position are persisted below
`.git/rebase-apply` through the repository filesystem adapter.

If a mail fails, `am_state()` reports its zero-based position. A caller can:

- resolve files, stage them, and call `continue_am()` to commit the stored mail
  metadata before processing later messages;
- call `skip_am()` to restore the current commit's tree and continue with the
  next message; or
- call `abort_am()` to restore the original ref, index, and tracked worktree.

Abort verifies that HEAD is still on the original branch or still detached,
preventing accidental reset of a branch selected while the operation was
paused. Unrelated untracked files survive abort. Unborn branches are restored
by removing AM-created tracked paths and deleting the newly created branch ref.

`AmOptions` bounds mail count, individual and aggregate mail bytes, header
bytes, parsing, patch application, object reads, and commits. AM always forces
forward indexed patch application and non-amend commits even if nested options
request check, reverse, or amend behavior.

Inline textual patches are supported. MIME multipart attachments and
quoted-printable patch bodies are transport concerns not accepted by this API;
callers can decode those containers to ordinary RFC 2822 headers plus inline
patch bytes first.

## Git source comparison

The state flow follows `am_run()` in `builtin/am.c`: mail is parsed into author,
message, and patch; apply runs against the index; a commit is created; and the
position advances only after success. Persistent recovery corresponds to Git's
`rebase-apply` state, `--continue`, `--skip`, and `--abort`. Header/address
handling is compared against `mailinfo.c`, while commit publication reuses the
same compare-and-swap path as `Repository::commit_index`.

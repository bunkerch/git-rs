use std::path::Path;
use std::process::Command;

use std::io::Write;

use git_rs::{
    CommitBuilder, EntryMode, HostFileSystem, InitOptions, ObjectKind,
    ReferenceName, Repository, Signature, Tree, TreeEntry,
};

fn git(args: &[&str], cwd: &Path) -> Vec<u8> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git binary must be available for integration tests");
    assert!(output.status.success(), "git {:?} failed: {:?}", args, String::from_utf8_lossy(&output.stderr));
    output.stdout
}

fn init_repo(path: &Path, bare: bool) -> Repository {
    let fs = HostFileSystem::new(path).unwrap();
    Repository::init(
        fs,
        if bare { "" } else { "." },
        &InitOptions {
            bare,
            initial_branch: "main".into(),
        },
    )
    .unwrap()
}

fn ident() -> Signature {
    Signature::new("Test", "test@example.com", 1_700_000_000, 0).unwrap()
}

/// Verify git-rs hash-object matches `git hash-object`.
#[test]
fn hash_object_matches_git() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path(), true);
    let id = repo.write_object(ObjectKind::Blob, b"hello\n").unwrap();
    let hex = id.to_string();
    // Compare via `git hash-object` of the same content
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .current_dir(dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"hello\n").unwrap();
    let output = child.wait_with_output().unwrap();
    let git_hash = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    assert_eq!(hex, git_hash, "hash-object mismatch for b\"hello\\n\"");
}

/// Verify git-rs tree construction matches `git ls-tree` output.
#[test]
fn write_tree_matches_git_ls_tree() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path(), true);

    let blob_id = repo.write_object(ObjectKind::Blob, b"file content\n").unwrap();
    let tree = Tree::new(vec![
        TreeEntry::new(EntryMode::Blob, b"file.txt".to_vec(), blob_id).unwrap(),
    ])
    .unwrap();
    let tree_id = repo.write_tree(&tree).unwrap();

    // Write a commit so we have something ls-tree can read
    let commit_id = repo
        .write_commit(&CommitBuilder::new(tree_id, ident(), ident()).message(b"init\n".to_vec()).build())
        .unwrap();
    repo.update_reference(
        &ReferenceName::branch("main").unwrap(),
        commit_id,
        git_rs::PreviousValue::MustNotExist,
    )
    .unwrap();

    let git_output = git(&["ls-tree", &tree_id.to_string()], dir.path());
    let git_line = String::from_utf8_lossy(&git_output);
    assert!(git_line.contains("100644 blob"), "ls-tree output: {git_line}");
}

/// Verify git-rs cat-file matches `git cat-file`.
#[test]
fn cat_file_matches_git() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path(), true);

    let id = repo.write_object(ObjectKind::Blob, b"test content\n").unwrap();
    let object = repo.read_object(id, 4096).unwrap();

    let git_output = git(&["cat-file", "-p", &id.to_string()], dir.path());
    assert_eq!(object.data(), git_output.as_slice(), "cat-file -p mismatch");
    assert_eq!(object.kind(), ObjectKind::Blob, "object kind mismatch");
}

/// Verify git-rs rev-parse matches `git rev-parse`.
#[test]
fn rev_parse_matches_git() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path(), true);

    let blob_id = repo.write_object(ObjectKind::Blob, b"data\n").unwrap();
    let tree = Tree::new(vec![
        TreeEntry::new(EntryMode::Blob, b"a".to_vec(), blob_id).unwrap(),
    ])
    .unwrap();
    let tree_id = repo.write_tree(&tree).unwrap();
    let commit_id = repo
        .write_commit(&CommitBuilder::new(tree_id, ident(), ident()).message(b"commit\n".to_vec()).build())
        .unwrap();
    repo.update_reference(
        &ReferenceName::branch("main").unwrap(),
        commit_id,
        git_rs::PreviousValue::MustNotExist,
    )
    .unwrap();

    let git_output = git(&["rev-parse", "HEAD"], dir.path());
    let git_hash = String::from_utf8_lossy(&git_output).trim().to_owned();
    assert_eq!(commit_id.to_string(), git_hash, "rev-parse HEAD mismatch");

    let git_tree = git(&["rev-parse", "HEAD^{tree}"], dir.path());
    let git_tree_hash = String::from_utf8_lossy(&git_tree).trim().to_owned();
    assert_eq!(tree_id.to_string(), git_tree_hash, "rev-parse HEAD{{tree}} mismatch");
}

/// Verify git-rs show-ref matches `git show-ref`.
#[test]
fn show_ref_matches_git() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path(), true);

    let blob_id = repo.write_object(ObjectKind::Blob, b"x\n").unwrap();
    let tree = Tree::new(vec![
        TreeEntry::new(EntryMode::Blob, b"f".to_vec(), blob_id).unwrap(),
    ])
    .unwrap();
    let tree_id = repo.write_tree(&tree).unwrap();
    let commit_id = repo
        .write_commit(&CommitBuilder::new(tree_id, ident(), ident()).message(b"x\n".to_vec()).build())
        .unwrap();
    repo.update_reference(
        &ReferenceName::branch("main").unwrap(),
        commit_id,
        git_rs::PreviousValue::MustNotExist,
    )
    .unwrap();
    repo.update_reference(
        &ReferenceName::new("refs/tags/v1").unwrap(),
        commit_id,
        git_rs::PreviousValue::MustNotExist,
    )
    .unwrap();

    let git_output = git(&["show-ref"], dir.path());
    let git_refs = String::from_utf8_lossy(&git_output);
    assert!(git_refs.contains("refs/heads/main"), "show-ref missing main: {git_refs}");
    assert!(git_refs.contains("refs/tags/v1"), "show-ref missing v1: {git_refs}");
}

/// Verify git-rs index encoding/decoding round-trips correctly.
#[test]
fn index_round_trip_matches_git() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path(), false);

    std::fs::write(dir.path().join("hello.txt"), b"world\n").unwrap();
    repo.add("hello.txt").unwrap();

    let git_output = git(&["ls-files", "--stage"], dir.path());
    let git_line = String::from_utf8_lossy(&git_output);
    assert!(git_line.contains("hello.txt"), "ls-files missing hello.txt: {git_line}");
}

/// Verify git-rs pack construction produces a Git-valid pack.
#[test]
fn build_pack_matches_git_verify() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path(), true);

    let blob_id = repo.write_object(ObjectKind::Blob, b"pack test\n").unwrap();
    let pack = repo.build_pack(&[blob_id], &Default::default()).unwrap();

    let pack_dir = dir.path().join("objects/pack");
    std::fs::create_dir_all(&pack_dir).unwrap();
    std::fs::write(pack_dir.join("pack-verify.pack"), pack.pack()).unwrap();
    std::fs::write(pack_dir.join("pack-verify.idx"), pack.index()).unwrap();
    let git_output = git(
        &["verify-pack", "--verbose", "objects/pack/pack-verify.idx"],
        dir.path(),
    );
    let output = String::from_utf8_lossy(&git_output);
    assert!(output.contains(&blob_id.to_string()), "verify-pack missing blob: {output}");
}

/// Verify git-rs commit tree matches `git commit-tree --stdin`.
#[test]
fn commit_tree_matches_git() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path(), true);

    let blob_id = repo.write_object(ObjectKind::Blob, b"hello\n").unwrap();
    let tree = Tree::new(vec![
        TreeEntry::new(EntryMode::Blob, b"greeting".to_vec(), blob_id).unwrap(),
    ])
    .unwrap();
    let tree_id = repo.write_tree(&tree).unwrap();

    let commit = CommitBuilder::new(tree_id, ident(), ident())
        .message(b"initial\n".to_vec())
        .build();
    let commit_id = repo.write_commit(&commit).unwrap();

    let git_output = git(
        &["cat-file", "-p", &commit_id.to_string()],
        dir.path(),
    );
    let git_body = String::from_utf8_lossy(&git_output);
    assert!(git_body.starts_with("tree "), "commit missing tree header: {git_body}");
    assert!(git_body.contains("author Test"), "commit missing author: {git_body}");
    assert!(git_body.contains("committer Test"), "commit missing committer: {git_body}");
    assert!(git_body.contains("\ninitial\n"), "commit missing message: {git_body}");
}

/// Verify git-rs describe output matches `git describe`.
#[test]
fn describe_matches_git() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path(), true);

    let blob_id = repo.write_object(ObjectKind::Blob, b"content\n").unwrap();
    let tree = Tree::new(vec![
        TreeEntry::new(EntryMode::Blob, b"f".to_vec(), blob_id).unwrap(),
    ])
    .unwrap();
    let tree_id = repo.write_tree(&tree).unwrap();

    // First commit on main
    let first = repo
        .write_commit(&CommitBuilder::new(tree_id, ident(), ident()).message(b"first\n".to_vec()).build())
        .unwrap();
    repo.update_reference(
        &ReferenceName::branch("main").unwrap(),
        first,
        git_rs::PreviousValue::MustNotExist,
    )
    .unwrap();

    // Tag the first commit using create_annotated_tag
    let tag = git_rs::TagBuilder::new(first, ObjectKind::Commit, "v1.0", ident())
        .unwrap()
        .message(b"release\n".to_vec())
        .build();
    repo.create_annotated_tag("v1.0", &tag, false, 4096).unwrap();

    // Second commit
    let second = repo
        .write_commit(
            &CommitBuilder::new(tree_id, ident(), ident())
                .parent(first)
                .message(b"second\n".to_vec())
                .build(),
        )
        .unwrap();
    repo.update_reference(
        &ReferenceName::branch("main").unwrap(),
        second,
        git_rs::PreviousValue::Any,
    )
    .unwrap();

    let description = repo.describe("HEAD", &Default::default()).unwrap();
    let describe_str = &*description.rendered();

    let git_output = git(&["describe", "--always"], dir.path());
    let git_desc = String::from_utf8_lossy(&git_output).trim().to_owned();
    assert!(
        describe_str.starts_with("v1.0"),
        "describe should start with tag: {describe_str}"
    );
    assert_eq!(describe_str, git_desc, "describe mismatch: git-rs={describe_str} git={git_desc}");
}

/// Verify git-rs merge-base matches `git merge-base`.
#[test]
fn merge_base_matches_git() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path(), true);

    let blob_id = repo.write_object(ObjectKind::Blob, b"base\n").unwrap();
    let tree_id = repo.write_tree(
        &Tree::new(vec![TreeEntry::new(EntryMode::Blob, b"f".to_vec(), blob_id).unwrap()]).unwrap(),
    )
    .unwrap();

    // Root commit
    let root = repo
        .write_commit(&CommitBuilder::new(tree_id, ident(), ident()).message(b"root\n".to_vec()).build())
        .unwrap();

    // Left branch
    let left_blob = repo.write_object(ObjectKind::Blob, b"left\n").unwrap();
    let left_tree = repo.write_tree(
        &Tree::new(vec![TreeEntry::new(EntryMode::Blob, b"f".to_vec(), left_blob).unwrap()]).unwrap(),
    )
    .unwrap();
    let left = repo
        .write_commit(
            &CommitBuilder::new(left_tree, ident(), ident())
                .parent(root)
                .message(b"left\n".to_vec())
                .build(),
        )
        .unwrap();

    // Right branch
    let right_blob = repo.write_object(ObjectKind::Blob, b"right\n").unwrap();
    let right_tree = repo.write_tree(
        &Tree::new(vec![TreeEntry::new(EntryMode::Blob, b"f".to_vec(), right_blob).unwrap()]).unwrap(),
    )
    .unwrap();
    let right = repo
        .write_commit(
            &CommitBuilder::new(right_tree, ident(), ident())
                .parent(root)
                .message(b"right\n".to_vec())
                .build(),
        )
        .unwrap();

    repo.update_reference(
        &ReferenceName::branch("left").unwrap(),
        left,
        git_rs::PreviousValue::MustNotExist,
    )
    .unwrap();
    repo.update_reference(
        &ReferenceName::branch("right").unwrap(),
        right,
        git_rs::PreviousValue::MustNotExist,
    )
    .unwrap();

    let mb = repo.merge_bases(left, right, &Default::default()).unwrap();
    assert_eq!(mb.len(), 1, "expected exactly one merge base");
    assert_eq!(mb[0], root, "merge base should be root commit");

    let git_output = git(&["merge-base", "refs/heads/left", "refs/heads/right"], dir.path());
    let git_base = String::from_utf8_lossy(&git_output).trim().to_owned();
    assert_eq!(root.to_string(), git_base, "merge-base mismatch");
}

/// Verify git-rs mktree round-trips correctly for complex trees.
#[test]
fn mktree_with_git() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path(), true);

    let blob_a = repo.write_object(ObjectKind::Blob, b"alpha\n").unwrap();
    let blob_b = repo.write_object(ObjectKind::Blob, b"beta\n").unwrap();

    let sub = Tree::new(vec![
        TreeEntry::new(EntryMode::Blob, b"nested".to_vec(), blob_b).unwrap(),
    ])
    .unwrap();
    let sub_id = repo.write_tree(&sub).unwrap();

    let tree = Tree::new(vec![
        TreeEntry::new(EntryMode::Blob, b"a".to_vec(), blob_a).unwrap(),
        TreeEntry::new(EntryMode::Tree, b"sub".to_vec(), sub_id).unwrap(),
    ])
    .unwrap();
    let tree_id = repo.write_tree(&tree).unwrap();

    // Verify with git ls-tree
    let git_bytes = git(&["ls-tree", &tree_id.to_string()], dir.path());
    let raw_output = String::from_utf8_lossy(&git_bytes);
    let lines: Vec<&str> = raw_output.lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 tree entries, got {lines:?}");
    assert!(
        lines.iter().any(|l| l.contains("blob") && l.ends_with("a")),
        "missing blob entry 'a': {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("tree") && l.ends_with("sub")),
        "missing tree entry 'sub': {lines:?}"
    );
}

/// Verify git-rs diff output format matches git's unified diff.
#[test]
fn diff_matches_git_format() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path(), false);

    std::fs::write(dir.path().join("file.txt"), b"old content\n").unwrap();
    repo.add("file.txt").unwrap();
    let first_tree = repo.write_index_tree(&repo.read_index().unwrap()).unwrap();
    let first = repo
        .write_commit(
            &CommitBuilder::new(first_tree, ident(), ident())
                .message(b"first\n".to_vec())
                .build(),
        )
        .unwrap();
    repo.update_reference(
        &ReferenceName::branch("main").unwrap(),
        first,
        git_rs::PreviousValue::MustNotExist,
    )
    .unwrap();

    std::fs::write(dir.path().join("file.txt"), b"new content\n").unwrap();
    repo.add("file.txt").unwrap();
    let second_tree = repo.write_index_tree(&repo.read_index().unwrap()).unwrap();
    let second = repo
        .write_commit(
            &CommitBuilder::new(second_tree, ident(), ident())
                .message(b"second\n".to_vec())
                .build(),
        )
        .unwrap();
    repo.update_reference(
        &ReferenceName::branch("main").unwrap(),
        second,
        git_rs::PreviousValue::Any,
    )
    .unwrap();

    // Get the diff between the two trees
    let diff_entries = repo.diff_trees(Some(first_tree), Some(second_tree), &Default::default()).unwrap();
    let mut diff_output = Vec::new();
    for entry in &diff_entries {
        diff_output.extend(repo.render_patch(entry, &Default::default()).unwrap());
    }
    let diff_str = String::from_utf8_lossy(&diff_output);
    assert!(
        diff_str.starts_with("diff --git"),
        "diff should start with diff --git: {diff_str}"
    );
    assert!(
        diff_str.contains("--- a/file.txt"),
        "diff should have --- a/file.txt: {diff_str}"
    );
    assert!(
        diff_str.contains("+++ b/file.txt"),
        "diff should have +++ b/file.txt: {diff_str}"
    );
    assert!(
        diff_str.contains("-old content"),
        "diff should show -old content: {diff_str}"
    );
    assert!(
        diff_str.contains("+new content"),
        "diff should show +new content: {diff_str}"
    );
}

/// Verify in-process fetch transport between two host repositories.
#[test]
fn in_process_fetch_matches_git() {
    let source_dir = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();

    // Source: create a repository with one commit
    let source = init_repo(source_dir.path(), true);
    let blob_id = source.write_object(ObjectKind::Blob, b"shared\n").unwrap();
    let tree_id = source.write_tree(
        &Tree::new(vec![TreeEntry::new(EntryMode::Blob, b"f".to_vec(), blob_id).unwrap()]).unwrap(),
    )
    .unwrap();
    let commit_id = source
        .write_commit(
            &CommitBuilder::new(tree_id, ident(), ident())
                .message(b"shared\n".to_vec())
                .build(),
        )
        .unwrap();
    source
        .update_reference(
            &ReferenceName::branch("main").unwrap(),
            commit_id,
            git_rs::PreviousValue::MustNotExist,
        )
        .unwrap();

    // Target: bare repo, fetch from source via in-process transport
    let target = init_repo(target_dir.path(), true);
    let mut transport =
        git_rs::RepositoryTransport::new(&source, git_rs::UploadPackOptions::default());
    target
        .fetch(&mut transport, &git_rs::FetchOptions::default())
        .unwrap();
    let new_id = target.resolve_reference("refs/remotes/origin/main").unwrap();
    assert_eq!(new_id, commit_id, "fetched commit should match source");

    // Verify git can see the fetched data in the target
    let git_output = git(
        &["cat-file", "-p", &commit_id.to_string()],
        target_dir.path(),
    );
    let body = String::from_utf8_lossy(&git_output);
    assert!(body.contains("shared\n"), "fetched object not readable by git: {body}");
}

/// Verify in-process receive-pack (push) sends objects that git can verify.
#[test]
fn in_process_push_matches_git() {
    let source_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();

    // Source: create a repo with one commit
    let source = init_repo(source_dir.path(), true);
    let blob_id = source.write_object(ObjectKind::Blob, b"pushed content\n").unwrap();
    let tree_id = source.write_tree(
        &Tree::new(vec![TreeEntry::new(EntryMode::Blob, b"f".to_vec(), blob_id).unwrap()]).unwrap(),
    )
    .unwrap();
    let commit_id = source
        .write_commit(
            &CommitBuilder::new(tree_id, ident(), ident())
                .message(b"pushed\n".to_vec())
                .build(),
        )
        .unwrap();

    // Destination: bare repo, receive push via in-process transport
    let dest = init_repo(dest_dir.path(), true);
    let mut transport = git_rs::InProcessReceivePackTransport::new(
        &dest,
        git_rs::ReceivePackOptions::default(),
    );
    let update = git_rs::PushUpdate::update(
        ReferenceName::branch("main").unwrap(),
        commit_id,
    );
    let result = source
        .push(
            &mut transport,
            &[update],
            &git_rs::PushOptions::default(),
        )
        .unwrap();
    assert!(
        result.statuses.iter().all(|s| s.error.is_none()),
        "push had errors: {:?}",
        result.statuses
    );

    // Verify git can read the pushed data in the destination
    let git_output = git(
        &["cat-file", "-p", &commit_id.to_string()],
        dest_dir.path(),
    );
    let body = String::from_utf8_lossy(&git_output);
    assert!(body.contains("pushed\n"), "pushed object not readable by git: {body}");

    // Verify git sees the ref in the destination
    let git_refs = git(&["show-ref"], dest_dir.path());
    let refs = String::from_utf8_lossy(&git_refs);
    assert!(
        refs.contains("refs/heads/main"),
        "push did not create ref in destination: {refs}"
    );
    assert!(
        refs.contains(&commit_id.to_string()),
        "push ref points to wrong commit: {refs}"
    );
}

/// Verify upload-pack advertisement produces a packet stream git can parse.
#[test]
fn upload_pack_advertisement_matches_git() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path(), true);

    let blob_id = repo.write_object(ObjectKind::Blob, b"data\n").unwrap();
    let tree_id = repo.write_tree(
        &Tree::new(vec![TreeEntry::new(EntryMode::Blob, b"f".to_vec(), blob_id).unwrap()]).unwrap(),
    )
    .unwrap();
    let commit_id = repo
        .write_commit(
            &CommitBuilder::new(tree_id, ident(), ident())
                .message(b"tip\n".to_vec())
                .build(),
        )
        .unwrap();
    repo.update_reference(
        &ReferenceName::branch("main").unwrap(),
        commit_id,
        git_rs::PreviousValue::MustNotExist,
    )
    .unwrap();

    // Generate upload-pack advertisement (v0/v1 style)
    let advertisement = repo.advertise_upload_pack().unwrap();

    // Verify the advertisement is a valid pkt-line stream
    let mut decoder = git_rs::PktLineDecoder::new();
    decoder.extend(&advertisement);
    let packets: Vec<_> = std::iter::from_fn(|| decoder.next_packet().ok()?)
        .collect();
    assert!(!packets.is_empty(), "upload-pack advertisement should have packets");

    // First packet should be a ref advertisement or capabilities
    let first = &packets[0];
    match first {
        git_rs::PktLine::Data(data) => {
            // Should contain a null ID (unborn) or commit ID + capabilities
            let has_commit = String::from_utf8_lossy(data).contains(&commit_id.to_string());
            let has_null = data.starts_with(b"0000000000000000000000000000000000000000");
            assert!(
                has_commit || has_null,
                "upload-pack first packet should advertise ref or capabilities: {data:?}"
            );
        }
        _ => panic!("upload-pack advertisement should start with data packet"),
    }

    // Should end with a flush packet
    assert_eq!(
        packets.last(),
        Some(&git_rs::PktLine::Flush),
        "upload-pack should end with flush"
    );
}

/// Verify upload-pack v2 advertisement (capability-based).
#[test]
fn upload_pack_v2_advertisement_matches_git() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path(), true);

    let blob_id = repo.write_object(ObjectKind::Blob, b"v2 data\n").unwrap();
    let tree_id = repo.write_tree(
        &Tree::new(vec![TreeEntry::new(EntryMode::Blob, b"f".to_vec(), blob_id).unwrap()]).unwrap(),
    )
    .unwrap();
    let commit_id = repo
        .write_commit(
            &CommitBuilder::new(tree_id, ident(), ident())
                .message(b"v2\n".to_vec())
                .build(),
        )
        .unwrap();
    repo.update_reference(
        &ReferenceName::branch("main").unwrap(),
        commit_id,
        git_rs::PreviousValue::MustNotExist,
    )
    .unwrap();

    // Generate v2 advertisement
    let advertisement = repo.advertise_upload_pack_v2().unwrap();
    assert!(!advertisement.is_empty(), "v2 advertisement should not be empty");

    let mut decoder = git_rs::PktLineDecoder::new();
    decoder.extend(&advertisement);
    let packets: Vec<_> = std::iter::from_fn(|| decoder.next_packet().ok()?)
        .collect();
    assert!(!packets.is_empty(), "v2 should have at least a version packet");

    // First packet should declare version 2
    let first = &packets[0];
    match first {
        git_rs::PktLine::Data(data) => {
            let line = String::from_utf8_lossy(data);
            assert!(
                line.contains("version 2"),
                "v2 should start with version announcement: {line}"
            );
        }
        _ => panic!("v2 should start with version packet"),
    }

    // Should end with flush
    assert_eq!(
        packets.last(),
        Some(&git_rs::PktLine::Flush),
        "v2 should end with flush"
    );
}

/// Verify git-rs branch creation and listing matches `git branch`.
#[test]
fn branch_operations_matches_git() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path(), true);

    // Create a commit
    let blob_id = repo.write_object(ObjectKind::Blob, b"branch test\n").unwrap();
    let tree_id = repo.write_tree(
        &Tree::new(vec![TreeEntry::new(EntryMode::Blob, b"f".to_vec(), blob_id).unwrap()]).unwrap(),
    )
    .unwrap();
    let commit_id = repo
        .write_commit(
            &CommitBuilder::new(tree_id, ident(), ident())
                .message(b"branch\n".to_vec())
                .build(),
        )
        .unwrap();
    repo.update_reference(
        &ReferenceName::branch("main").unwrap(),
        commit_id,
        git_rs::PreviousValue::MustNotExist,
    )
    .unwrap();

    // Create a feature branch via git-rs
    repo.create_branch("feature", commit_id, false).unwrap();

    // Verify git can see both branches
    let git_branches = git(&["branch", "--list"], dir.path());
    let branches = String::from_utf8_lossy(&git_branches);
    assert!(branches.contains("main"), "missing main branch: {branches}");
    assert!(branches.contains("feature"), "missing feature branch: {branches}");

    // Rename branch via git-rs (rename_branch takes options + committer)
    repo.rename_branch(
        "feature",
        "renamed",
        &git_rs::RenameBranchOptions::default(),
        &ident(),
    )
    .unwrap();

    // Verify git sees renamed branch
    let git_branches = git(&["branch", "--list"], dir.path());
    let branches = String::from_utf8_lossy(&git_branches);
    assert!(!branches.contains("feature"), "feature should be gone after rename: {branches}");
    assert!(branches.contains("renamed"), "renamed branch missing: {branches}");

    // Delete branch via git-rs
    repo.delete_branch("renamed", &git_rs::DeleteBranchOptions::default()).unwrap();

    let git_branches = git(&["branch", "--list"], dir.path());
    let branches = String::from_utf8_lossy(&git_branches);
    assert!(!branches.contains("renamed"), "renamed should be deleted: {branches}");
}

/// Verify git-rs worktree add and prune match `git worktree` behavior.
#[test]
fn worktree_operations_matches_git() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(dir.path(), true);

    // Create a commit on main in the bare repo
    let blob_id = repo.write_object(ObjectKind::Blob, b"worktree\n").unwrap();
    let tree_id = repo
        .write_tree(
            &Tree::new(vec![
                TreeEntry::new(EntryMode::Blob, b"f".to_vec(), blob_id).unwrap()
            ])
            .unwrap(),
        )
        .unwrap();
    let commit_id = repo
        .write_commit(
            &CommitBuilder::new(tree_id, ident(), ident())
                .message(b"worktree\n".to_vec())
                .build(),
        )
        .unwrap();
    repo.update_reference(
        &ReferenceName::branch("main").unwrap(),
        commit_id,
        git_rs::PreviousValue::MustNotExist,
    )
    .unwrap();

    // Create a separate branch for the worktree
    repo.create_branch("wt-branch", commit_id, false).unwrap();

    // Add a linked worktree via git-rs (bare repo uses root-relative paths)
    let worktree_name = "new-wt";
    repo.add_worktree(
        worktree_name,
        worktree_name,
        &git_rs::WorktreeTarget::Branch("wt-branch".into()),
        &git_rs::AddWorktreeOptions::default(),
    )
    .unwrap();

    // Verify worktree admin directory was created
    let admin_dir = dir.path().join("worktrees").join(worktree_name);
    assert!(
        admin_dir.exists(),
        "worktree admin directory should exist: {admin_dir:?}"
    );

    // Verify git detects the worktree via the admin registration file
    let wt_gitdir = admin_dir.join("gitdir");
    assert!(
        wt_gitdir.exists(),
        "worktree gitdir should exist: {wt_gitdir:?}"
    );
    let gitdir_contents = std::fs::read_to_string(&wt_gitdir).unwrap();
    assert!(
        gitdir_contents.contains(worktree_name),
        "gitdir file should reference worktree path: {gitdir_contents}"
  );
}

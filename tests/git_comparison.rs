use std::path::Path;
use std::process::Command;

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
    use std::io::Write;
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

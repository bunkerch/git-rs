use std::env;
use std::time::{SystemTime, UNIX_EPOCH};

use git_rs::{
    CommitBuilder, HostFileSystem, PreviousValue, ReferenceTarget, Repository, Signature, Tree,
};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let message = env::args()
        .nth(2)
        .unwrap_or_else(|| "empty commit\n".to_owned());
    let name = env::var("GIT_AUTHOR_NAME").unwrap_or_else(|_| "git-rs".to_owned());
    let email = env::var("GIT_AUTHOR_EMAIL").unwrap_or_else(|_| "git-rs@example.com".to_owned());
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| git_rs::Error::InvalidCommit(error.to_string()))?
        .as_secs()
        .try_into()
        .map_err(|_| git_rs::Error::InvalidCommit("current timestamp exceeds i64".into()))?;

    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let head = repository.read_reference("HEAD")?;
    let ReferenceTarget::Symbolic(branch) = head.target() else {
        return Err(git_rs::Error::InvalidCommit(
            "detached HEAD is not supported by this example".into(),
        ));
    };
    let previous = match repository.resolve_reference("HEAD") {
        Ok(parent) => PreviousValue::MustExist(parent),
        Err(git_rs::Error::NotFound(_)) => PreviousValue::MustNotExist,
        Err(error) => return Err(error),
    };
    let identity = Signature::new(name, email, timestamp, 0)?;
    let tree = repository.write_tree(&Tree::default())?;
    let mut builder =
        CommitBuilder::new(tree, identity.clone(), identity).message(message.into_bytes());
    if let PreviousValue::MustExist(parent) = previous {
        builder = builder.parent(parent);
    }
    let commit = repository.write_commit(&builder.build())?;
    repository.update_reference(branch, commit, previous)?;
    println!("{commit}");
    Ok(())
}

use std::env;

use git_rs::{
    HostFileSystem, Repository, RepositoryTransport, SubmoduleAddOptions, UploadPackOptions,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let superproject_path = arguments
        .next()
        .ok_or("usage: submodule_add_local <superproject> <remote> <path>")?;
    let remote_path = arguments
        .next()
        .ok_or("usage: submodule_add_local <superproject> <remote> <path>")?;
    let path = arguments
        .next()
        .ok_or("usage: submodule_add_local <superproject> <remote> <path>")?;
    if arguments.next().is_some() {
        return Err("usage: submodule_add_local <superproject> <remote> <path>".into());
    }
    let filesystem = HostFileSystem::new(".")?;
    let superproject = Repository::open(filesystem.clone(), superproject_path)?;
    let remote = Repository::open(filesystem, &remote_path)?;
    let mut transport = RepositoryTransport::new(&remote, UploadPackOptions::default());
    let report = superproject.add_submodule(
        path.as_bytes(),
        remote_path.as_bytes(),
        &mut transport,
        &SubmoduleAddOptions::default(),
    )?;
    println!(
        "{} {}",
        report.gitlink,
        String::from_utf8_lossy(report.module.path())
    );
    Ok(())
}

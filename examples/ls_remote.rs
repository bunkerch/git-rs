use std::env;
use std::io::{self, Write};

use git_rs::{
    HostFileSystem, LsRemoteOptions, LsRemoteSelection, Repository, RepositoryTransport,
    RepositoryV2Transport, UploadPackOptions, UploadPackV2Limits, ls_remote, ls_remote_v2,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().ok_or(
        "usage: ls_remote REPOSITORY [--v2] [--branches|--tags] [--refs] [--symref] [PATTERN...]",
    )?;
    let mut options = LsRemoteOptions::default();
    let mut v2 = false;
    let mut branches = false;
    let mut tags = false;
    for argument in arguments {
        match argument.as_str() {
            "--v2" => v2 = true,
            "--branches" => branches = true,
            "--tags" => tags = true,
            "--refs" => options.refs_only = true,
            "--symref" => options.show_symrefs = true,
            _ if argument.starts_with('-') => {
                return Err(format!("unknown option: {argument}").into());
            }
            _ => options.patterns.push(argument),
        }
    }
    options.selection = match (branches, tags) {
        (false, false) => LsRemoteSelection::All,
        (true, false) => LsRemoteSelection::Branches,
        (false, true) => LsRemoteSelection::Tags,
        (true, true) => LsRemoteSelection::BranchesAndTags,
    };
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    let result = if v2 {
        let mut transport = RepositoryV2Transport::new(
            &repository,
            UploadPackOptions::default(),
            UploadPackV2Limits::default(),
        );
        ls_remote_v2(&mut transport, &options)?
    } else {
        let mut transport = RepositoryTransport::new(&repository, UploadPackOptions::default());
        ls_remote(&mut transport, &options)?
    };
    io::stdout().write_all(result.output())?;
    Ok(())
}

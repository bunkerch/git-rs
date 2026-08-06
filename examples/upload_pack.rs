use std::env;
use std::io::{self, Read, Write};

use git_rs::{HostFileSystem, Repository, UploadPackOptions, UploadPackRequest};

fn main() -> git_rs::Result<()> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .expect("usage: upload_pack <repository> [--advertise]");
    let advertise = arguments.next().as_deref() == Some("--advertise");
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let output = if advertise {
        repository.advertise_upload_pack()?
    } else {
        let mut input = Vec::new();
        io::stdin().read_to_end(&mut input)?;
        let request = UploadPackRequest::parse(&input)?;
        repository.respond_upload_pack(&request, &UploadPackOptions::default())?
    };
    io::stdout().write_all(&output)?;
    Ok(())
}

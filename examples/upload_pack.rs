use std::env;
use std::io::{self, Read, Write};

use git_rs::{
    HostFileSystem, Repository, UploadPackOptions, UploadPackRequest, UploadPackV2Limits,
    UploadPackV2Request,
};

fn main() -> git_rs::Result<()> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .expect("usage: upload_pack <repository> [--advertise|--v2-advertise|--v2]");
    let mode = arguments.next();
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let output = match mode.as_deref() {
        Some("--advertise") => repository.advertise_upload_pack()?,
        Some("--v2-advertise") => repository.advertise_upload_pack_v2()?,
        Some("--v2") => {
            let mut input = Vec::new();
            io::stdin().read_to_end(&mut input)?;
            let limits = UploadPackV2Limits::default();
            let request = UploadPackV2Request::parse(&input, &limits)?;
            repository.respond_upload_pack_v2(&request, &UploadPackOptions::default(), &limits)?
        }
        None => {
            let mut input = Vec::new();
            io::stdin().read_to_end(&mut input)?;
            let request = UploadPackRequest::parse(&input)?;
            repository.respond_upload_pack(&request, &UploadPackOptions::default())?
        }
        Some(_) => {
            return Err(git_rs::Error::Protocol("unknown upload-pack mode".into()));
        }
    };
    io::stdout().write_all(&output)?;
    Ok(())
}

use std::env;
use std::io::{self, Read, Write};

use git_rs::{HostFileSystem, ReceivePackOptions, ReceivePackRequest, Repository};

fn main() -> git_rs::Result<()> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .expect("usage: receive_pack <repository> [--advertise]");
    let advertise = arguments.next().as_deref() == Some("--advertise");
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let output = if advertise {
        repository.advertise_receive_pack()?
    } else {
        let mut input = Vec::new();
        io::stdin().read_to_end(&mut input)?;
        let request = ReceivePackRequest::parse(&input)?;
        repository
            .receive_pack(&request, &ReceivePackOptions::default())?
            .response
    };
    io::stdout().write_all(&output)?;
    Ok(())
}

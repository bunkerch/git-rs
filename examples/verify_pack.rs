use std::env;

use git_rs::{Error, HostFileSystem, Repository, VerifyPackOptions};

fn main() -> git_rs::Result<()> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or_else(|| Error::InvalidRepository("usage: verify_pack REPOSITORY PACK.idx".into()))?;
    let index_path = arguments
        .next()
        .ok_or_else(|| Error::InvalidRepository("usage: verify_pack REPOSITORY PACK.idx".into()))?;
    let repository = Repository::open(HostFileSystem::new(".")?, repository_path)?;
    let report = repository.verify_pack(index_path, &VerifyPackOptions::default())?;
    for object in report.objects {
        print!(
            "{} {:?} {} {} {}",
            object.id, object.kind, object.size, object.packed_size, object.offset
        );
        if let Some(base) = object.base {
            print!(" {} {base}", object.delta_depth);
        }
        println!();
    }
    for (depth, count) in report.delta_histogram {
        println!("chain length = {depth}: {count} object(s)");
    }
    Ok(())
}

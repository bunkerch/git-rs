use std::env;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use git_rs::{BisectMark, BisectOptions, HostFileSystem, ObjectId, Repository, Signature};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: bisect <repository> <start|good|bad|skip|state|reset> ...")?;
    let command = arguments
        .next()
        .ok_or("usage: bisect <repository> <start|good|bad|skip|state|reset> ...")?;
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    let timestamp = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())?;
    let identity = Signature::new("git-rs", "git-rs@example.com", timestamp, 0)?;
    let mut options = BisectOptions::default();
    let outcome = match command.as_str() {
        "start" => {
            let bad =
                ObjectId::from_str(&arguments.next().ok_or("start requires bad and good IDs")?)?;
            let mut good = Vec::new();
            for argument in arguments {
                if argument == "--no-checkout" {
                    options.no_checkout = true;
                } else {
                    good.push(ObjectId::from_str(&argument)?);
                }
            }
            Some(repository.start_bisect(bad, &good, &options, &identity)?)
        }
        "good" | "bad" | "skip" => {
            options.no_checkout = repository
                .filesystem()
                .exists(&repository.git_dir().join("BISECT_HEAD"))?;
            let mark = match command.as_str() {
                "good" => BisectMark::Good,
                "bad" => BisectMark::Bad,
                _ => BisectMark::Skip,
            };
            Some(repository.mark_bisect(mark, None, &options, &identity)?)
        }
        "state" => {
            println!("{:?}", repository.bisect_state()?);
            None
        }
        "reset" => {
            repository.reset_bisect(&identity, options.graph.max_object_size)?;
            None
        }
        _ => return Err("unknown bisect command".into()),
    };
    if let Some(outcome) = outcome {
        println!("{outcome:?}");
    }
    Ok(())
}

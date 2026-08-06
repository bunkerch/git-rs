use std::env;
use std::io::{self, Read};
use std::str::FromStr;

use git_rs::{
    CommitTreeMessagePart, CommitTreeOptions, HostFileSystem, ObjectId, Repository, Signature,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().ok_or(
        "usage: commit_tree REPOSITORY TREE NAME EMAIL TIMESTAMP OFFSET [PARENT...] < MESSAGE",
    )?;
    let tree = ObjectId::from_str(&arguments.next().ok_or("missing tree")?)?;
    let name = arguments.next().ok_or("missing identity name")?;
    let email = arguments.next().ok_or("missing identity email")?;
    let timestamp = arguments
        .next()
        .ok_or("missing timestamp")?
        .parse::<i64>()?;
    let offset = parse_offset(&arguments.next().ok_or("missing timezone offset")?)?;
    let parents = arguments
        .map(|value| ObjectId::from_str(&value))
        .collect::<Result<Vec<_>, _>>()?;
    let mut message = Vec::new();
    io::stdin().read_to_end(&mut message)?;
    let identity = Signature::new(name, email, timestamp, offset)?;
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    let id = repository.commit_tree(
        tree,
        &parents,
        &[CommitTreeMessagePart::FileContents(message)],
        &identity,
        &identity,
        &CommitTreeOptions::default(),
    )?;
    println!("{id}");
    Ok(())
}

fn parse_offset(value: &str) -> Result<i16, Box<dyn std::error::Error>> {
    let bytes = value.as_bytes();
    if bytes.len() != 5
        || !matches!(bytes[0], b'+' | b'-')
        || !bytes[1..].iter().all(u8::is_ascii_digit)
    {
        return Err("timezone must have +HHMM or -HHMM form".into());
    }
    let hours = i16::from(bytes[1] - b'0') * 10 + i16::from(bytes[2] - b'0');
    let minutes = i16::from(bytes[3] - b'0') * 10 + i16::from(bytes[4] - b'0');
    if minutes >= 60 {
        return Err("timezone minutes exceed 59".into());
    }
    let offset = hours * 60 + minutes;
    Ok(if bytes[0] == b'-' { -offset } else { offset })
}

use std::env;
use std::str::FromStr;

use git_rs::{AttributeSource, AttributeValue, CheckAttributesOptions, HostFileSystem, Repository};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().unwrap_or_else(|| ".".into());
    let mut path = arguments
        .next()
        .ok_or("usage: check_attributes REPOSITORY [--cached|--tree TREE] PATH [ATTRIBUTE ...]")?;
    let source = if path == "--cached" {
        path = arguments.next().ok_or("--cached requires a path")?;
        AttributeSource::Index
    } else if path == "--tree" {
        let tree = git_rs::ObjectId::from_str(&arguments.next().ok_or("--tree requires an ID")?)?;
        path = arguments.next().ok_or("--tree requires a path")?;
        AttributeSource::Tree(tree)
    } else {
        AttributeSource::WorktreeThenIndex
    };
    let attributes = arguments.collect();
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    for result in repository.check_attributes(
        &[path.into_bytes()],
        &CheckAttributesOptions {
            source,
            attributes,
            ..CheckAttributesOptions::default()
        },
    )? {
        let value = match result.value() {
            AttributeValue::Set => "set",
            AttributeValue::Unset => "unset",
            AttributeValue::Value(value) => value,
            AttributeValue::Unspecified => "unspecified",
        };
        println!(
            "{}: {}: {value}",
            String::from_utf8_lossy(result.path()),
            result.name()
        );
    }
    Ok(())
}

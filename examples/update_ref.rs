use std::env;
use std::str::FromStr;

use git_rs::{
    HostFileSystem, ObjectId, ReferenceName, Repository, UpdateRefCommand, UpdateRefOptions,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().ok_or(
        "usage: update_ref REPOSITORY (update|create|delete|verify|symref-create) REF [VALUE] [OLD] [--no-deref]",
    )?;
    let operation = arguments.next().ok_or("missing operation")?;
    let name = arguments.next().ok_or("missing reference name")?;
    let values = arguments.collect::<Vec<_>>();
    let no_deref = values.iter().any(|value| value == "--no-deref");
    let values = values
        .iter()
        .filter(|value| value.as_str() != "--no-deref")
        .collect::<Vec<_>>();
    let command = match operation.as_str() {
        "update" => UpdateRefCommand::Update {
            name,
            new: ObjectId::from_str(values.first().ok_or("missing new object")?)?,
            old: values
                .get(1)
                .map(|value| ObjectId::from_str(value))
                .transpose()?,
            no_deref,
        },
        "create" => UpdateRefCommand::Create {
            name,
            new: ObjectId::from_str(values.first().ok_or("missing new object")?)?,
            no_deref,
        },
        "delete" => UpdateRefCommand::Delete {
            name,
            old: values
                .first()
                .map(|value| ObjectId::from_str(value))
                .transpose()?,
            no_deref,
        },
        "verify" => UpdateRefCommand::Verify {
            name,
            old: values
                .first()
                .map(|value| ObjectId::from_str(value))
                .transpose()?,
            no_deref,
        },
        "symref-create" => UpdateRefCommand::SymbolicCreate {
            name,
            new: ReferenceName::new((*values.first().ok_or("missing symbolic target")?).clone())?,
        },
        _ => return Err(format!("unknown operation: {operation}").into()),
    };
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    repository.update_refs(&[command], &UpdateRefOptions::default())?;
    Ok(())
}

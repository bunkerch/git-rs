use std::{env, path::Path};

use git_rs::{HostFileSystem, Repository, SparseCheckoutOptions};

fn main() -> git_rs::Result<()> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().unwrap_or_else(|| ".".to_owned());
    let command = arguments.next().unwrap_or_else(|| "list".to_owned());
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let repository = Repository::open(
        HostFileSystem::new(repository_path.parent().expect("repository parent"))?,
        repository_path.file_name().expect("repository name"),
    )?;
    match command.as_str() {
        "list" => {
            let state = repository.sparse_checkout_state(1_000_000, 64 * 1024 * 1024)?;
            for rule in state.rules() {
                println!("{}", String::from_utf8_lossy(rule));
            }
        }
        "disable" => {
            let report = repository.disable_sparse_checkout(&SparseCheckoutOptions::default())?;
            println!("materialized={}", report.materialized.len());
        }
        "set" => {
            let rules = arguments.map(String::into_bytes).collect::<Vec<_>>();
            let report =
                repository.set_sparse_checkout(&rules, &SparseCheckoutOptions::default())?;
            println!(
                "included={} excluded={} removed={} retained={}",
                report.included,
                report.excluded,
                report.removed.len(),
                report.retained.len()
            );
        }
        _ => {
            return Err(git_rs::Error::InvalidRepository(
                "expected list, set, or disable".into(),
            ));
        }
    }
    Ok(())
}

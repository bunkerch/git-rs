use std::env;
use std::io::{self, Write};

use git_rs::{CheckoutIndexOptions, CheckoutIndexStage, HostFileSystem, Repository};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().ok_or(
        "usage: checkout_index REPOSITORY [--all] [--force] [--no-create] [--update-stat] [--temp] [--prefix=VALUE] [--stage=0|1|2|3|all] [PATH...]",
    )?;
    let mut options = CheckoutIndexOptions::default();
    let mut paths = Vec::new();
    for argument in arguments {
        match argument.as_str() {
            "--all" => options.all = true,
            "--force" => options.force = true,
            "--no-create" => options.no_create = true,
            "--update-stat" => options.update_stat = true,
            "--temp" => options.temporary = true,
            "--ignore-skip-worktree-bits" => options.ignore_skip_worktree = true,
            "--stage=0" => options.stage = CheckoutIndexStage::Normal,
            "--stage=1" => options.stage = CheckoutIndexStage::Base,
            "--stage=2" => options.stage = CheckoutIndexStage::Ours,
            "--stage=3" => options.stage = CheckoutIndexStage::Theirs,
            "--stage=all" => options.stage = CheckoutIndexStage::AllConflicts,
            _ if argument.starts_with("--prefix=") => {
                options.prefix = argument.as_bytes()["--prefix=".len()..].to_vec();
            }
            _ if argument.starts_with('-') => {
                return Err(format!("unknown option: {argument}").into());
            }
            _ => paths.push(argument.into_bytes()),
        }
    }
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    let result = repository.checkout_index(&paths, &options)?;
    io::stdout().write_all(result.output())?;
    Ok(())
}

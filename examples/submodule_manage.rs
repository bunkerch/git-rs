use std::{env, path::Path};

use git_rs::{HostFileSystem, Repository, SubmoduleDeinitOptions, SubmoduleOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments
        .next()
        .ok_or("usage: submodule_manage <repository> <deinit|sync> [path] [--force]")?;
    let command = arguments
        .next()
        .ok_or("usage: submodule_manage <repository> <deinit|sync> [path] [--force]")?;
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let repository = Repository::open(
        HostFileSystem::new(repository_path.parent().ok_or("repository has no parent")?)?,
        repository_path
            .file_name()
            .ok_or("repository has no name")?,
    )?;
    match command.as_str() {
        "deinit" => {
            let path = arguments.next().ok_or("deinit requires a path")?;
            let force = arguments.next().as_deref() == Some("--force");
            let report = repository.deinit_submodule(
                path.as_bytes(),
                &SubmoduleDeinitOptions {
                    force,
                    ..SubmoduleDeinitOptions::default()
                },
            )?;
            println!("removed {} entries", report.removed_entries);
        }
        "sync" => {
            let paths = arguments.map(String::into_bytes).collect::<Vec<_>>();
            let synchronized = repository.sync_submodules(&SubmoduleOptions {
                paths,
                ..SubmoduleOptions::default()
            })?;
            println!("synchronized {} submodules", synchronized.len());
        }
        _ => return Err("expected deinit or sync".into()),
    }
    Ok(())
}

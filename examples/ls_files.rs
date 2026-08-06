use std::env;

use git_rs::{HostFileSystem, LsFilesOptions, Repository, Result};

fn main() -> Result<()> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().ok_or_else(|| {
        git_rs::Error::InvalidRevision(
            "usage: ls_files <repository> [-c] [-s] [-u] [-d] [-m] [-o] [-i] [-k] [--exclude-standard] [path...]".into(),
        )
    })?;
    let mut options = LsFilesOptions::default();
    for argument in arguments {
        match argument.as_str() {
            "-c" => options.cached = true,
            "-s" => options.stage = true,
            "-u" => options.unmerged = true,
            "-d" => options.deleted = true,
            "-m" => options.modified = true,
            "-o" => options.others = true,
            "-i" => options.ignored = true,
            "-k" => options.killed = true,
            "--exclude-standard" => options.exclude_standard = true,
            "--sparse" => options.show_sparse_directories = true,
            _ => options.paths.push(argument.into_bytes()),
        }
    }
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    for entry in repository.ls_files(&options)? {
        if options.stage || options.unmerged {
            println!(
                "{:06o} {} {}\t{}",
                entry.mode().unwrap_or_default(),
                entry.id().map_or_else(String::new, |id| id.to_string()),
                entry.stage().unwrap_or_default(),
                String::from_utf8_lossy(entry.path())
            );
        } else {
            println!("{}", String::from_utf8_lossy(entry.path()));
        }
    }
    Ok(())
}

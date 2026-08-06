use std::env;

use git_rs::{GrepBinaryMode, GrepOptions, GrepTarget, HostFileSystem, Repository, Result};

fn main() -> Result<()> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().ok_or_else(|| {
        git_rs::Error::InvalidRevision(
            "usage: grep <repository> <fixed-pattern> [--cached|--treeish=<rev>] [-i] [-v] [-w] [-a|-I] [path...]".into(),
        )
    })?;
    let pattern = arguments
        .next()
        .ok_or_else(|| git_rs::Error::InvalidRevision("grep requires a fixed pattern".into()))?;
    let mut options = GrepOptions::default();
    for argument in arguments {
        match argument.as_str() {
            "--cached" => options.target = GrepTarget::Index,
            "-i" => options.ignore_case = true,
            "-v" => options.invert_match = true,
            "-w" => options.word_regexp = true,
            "-a" => options.binary = GrepBinaryMode::Text,
            "-I" => options.binary = GrepBinaryMode::WithoutMatch,
            _ if argument.starts_with("--treeish=") => {
                options.target = GrepTarget::Treeish(argument[10..].to_owned());
            }
            _ => options.paths.push(argument.into_bytes()),
        }
    }
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    for hit in repository.grep_fixed(&[pattern.into_bytes()], &options)? {
        if hit.is_binary() {
            println!(
                "Binary file {} matches",
                String::from_utf8_lossy(hit.path())
            );
        } else {
            println!(
                "{}:{}:{}",
                String::from_utf8_lossy(hit.path()),
                hit.line_number(),
                String::from_utf8_lossy(hit.line())
            );
        }
    }
    Ok(())
}

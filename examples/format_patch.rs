use std::{env, path::Path, str::FromStr};

use git_rs::{FormatPatchOptions, HostFileSystem, ObjectId, Repository};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let usage = "usage: format_patch REPOSITORY TIP EXCLUDE OUTPUT_DIRECTORY";
    let tip = ObjectId::from_str(&env::args().nth(2).expect(usage))?;
    let exclude = ObjectId::from_str(&env::args().nth(3).expect(usage))?;
    let output_directory = env::args().nth(4).expect(usage);
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let repository = Repository::open(
        HostFileSystem::new(repository_path.parent().expect("repository parent"))?,
        repository_path.file_name().expect("repository name"),
    )?;
    let patches = repository.format_patches(&[tip], &[exclude], &FormatPatchOptions::default())?;
    std::fs::create_dir_all(&output_directory)?;
    for patch in patches {
        let path = Path::new(&output_directory).join(patch.filename());
        std::fs::write(&path, patch.data())?;
        println!("{}", path.display());
    }
    Ok(())
}

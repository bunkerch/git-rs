use std::env;
use std::fs;
use std::io::Write;

use git_rs::{MergeFileFavor, MergeFileOptions, MergeFileStyle, merge_file};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments.len() < 3 {
        return Err("usage: merge_file CURRENT BASE OTHER [--diff3|--zdiff3] [--ours|--theirs|--union] [--marker-size=N]".into());
    }
    let mut options = MergeFileOptions {
        current_label: arguments[0].as_bytes().to_vec(),
        base_label: arguments[1].as_bytes().to_vec(),
        other_label: arguments[2].as_bytes().to_vec(),
        ..MergeFileOptions::default()
    };
    for argument in &arguments[3..] {
        match argument.as_str() {
            "--diff3" => options.style = MergeFileStyle::Diff3,
            "--zdiff3" => options.style = MergeFileStyle::ZDiff3,
            "--ours" => options.favor = MergeFileFavor::Ours,
            "--theirs" => options.favor = MergeFileFavor::Theirs,
            "--union" => options.favor = MergeFileFavor::Union,
            value if value.starts_with("--marker-size=") => {
                options.marker_size = value[14..].parse()?;
            }
            _ => return Err(format!("unknown option `{argument}`").into()),
        }
    }
    let result = merge_file(
        &fs::read(&arguments[0])?,
        &fs::read(&arguments[1])?,
        &fs::read(&arguments[2])?,
        &options,
    )?;
    std::io::stdout().write_all(result.data())?;
    Ok(())
}

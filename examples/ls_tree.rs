use std::env;
use std::fmt::Write;

use git_rs::{HostFileSystem, Result};
use git_rs::{LsTreeOptions, ObjectKind, Repository};

fn main() -> Result<()> {
    let mut arguments = env::args().skip(1);
    let repository_path = arguments.next().ok_or_else(|| {
        git_rs::Error::InvalidRevision(
            "usage: ls_tree <repository> <tree-ish> [-r] [-t] [-l] [path...]".into(),
        )
    })?;
    let treeish = arguments.next().ok_or_else(|| {
        git_rs::Error::InvalidRevision(
            "usage: ls_tree <repository> <tree-ish> [-r] [-t] [-l] [path...]".into(),
        )
    })?;
    let mut options = LsTreeOptions::default();
    for argument in arguments {
        match argument.as_str() {
            "-r" => options.recursive = true,
            "-t" => options.show_trees = true,
            "-d" => options.trees_only = true,
            "-l" => options.include_object_size = true,
            _ => options.paths.push(argument.into_bytes()),
        }
    }
    let repository = Repository::open(HostFileSystem::new(repository_path)?, ".")?;
    for entry in repository.ls_tree(&treeish, &options)? {
        let kind = match entry.kind() {
            ObjectKind::Blob => "blob",
            ObjectKind::Tree => "tree",
            ObjectKind::Commit => "commit",
            ObjectKind::Tag => "tag",
        };
        let path = quote_c_style(entry.path());
        if options.include_object_size {
            let size = entry
                .object_size()
                .map_or_else(|| "-".into(), |size| size.to_string());
            println!(
                "{:06o} {kind} {} {:>7}\t{path}",
                entry.mode_number(),
                entry.id(),
                size,
            );
        } else {
            println!(
                "{:06o} {kind} {}\t{path}",
                entry.mode_number(),
                entry.id(),
            );
        }
    }
    Ok(())
}

fn quote_c_style(path: &[u8]) -> String {
    let mut escaped = String::new();
    for c in String::from_utf8_lossy(path).chars() {
        match c {
            '\x07' => escaped.push_str(r"\a"),
            '\x08' => escaped.push_str(r"\b"),
            '\t' => escaped.push_str(r"\t"),
            '\n' => escaped.push_str(r"\n"),
            '\x0b' => escaped.push_str(r"\v"),
            '\x0c' => escaped.push_str(r"\f"),
            '\r' => escaped.push_str(r"\r"),
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str(r"\\"),
            c @ ('\u{0}'..='\u{1f}' | '\x7f') => {
                let _ = write!(escaped, "\\{:03o}", c as u32);
            }
            c => escaped.push(c),
        }
    }
    escaped
}

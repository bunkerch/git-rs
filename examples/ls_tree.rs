use std::env;

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

/// C-style quote path bytes the way git's `quote_c_style` does, so control
/// bytes can never reach the terminal raw. Every byte below 0x20, DEL, and
/// every high byte (including the C1 range 0x80-0x9f) is rendered as a
/// three-digit octal escape; `"` and `\` are escaped as well, and the result
/// is wrapped in double quotes when any byte needed escaping.
fn quote_c_style(path: &[u8]) -> String {
    let needs_quote = path
        .iter()
        .any(|&byte| matches!(byte, 0x00..=0x1f | b'"' | b'\\' | 0x7f..=0xff));
    let mut escaped = String::new();
    if needs_quote {
        escaped.push('"');
    }
    for &byte in path {
        match byte {
            b'\x07' => escaped.push_str(r"\a"),
            b'\x08' => escaped.push_str(r"\b"),
            b'\t' => escaped.push_str(r"\t"),
            b'\n' => escaped.push_str(r"\n"),
            b'\x0b' => escaped.push_str(r"\v"),
            b'\x0c' => escaped.push_str(r"\f"),
            b'\r' => escaped.push_str(r"\r"),
            b'"' => escaped.push_str("\\\""),
            b'\\' => escaped.push_str(r"\\"),
            0x00..=0x1f | 0x7f..=0xff => push_octal(&mut escaped, byte),
            0x20..=0x7e => escaped.push(char::from(byte)),
        }
    }
    if needs_quote {
        escaped.push('"');
    }
    escaped
}

fn push_octal(escaped: &mut String, byte: u8) {
    escaped.push('\\');
    escaped.push(char::from(b'0' + (byte >> 6)));
    escaped.push(char::from(b'0' + ((byte >> 3) & 7)));
    escaped.push(char::from(b'0' + (byte & 7)));
}

#[cfg(test)]
mod tests {
    use super::quote_c_style;

    #[test]
    fn leaves_printable_ascii_unaltered() {
        assert_eq!(quote_c_style(b"dir/file.txt"), "dir/file.txt");
    }

    #[test]
    fn escapes_ascii_control_bytes_like_git() {
        assert_eq!(
            quote_c_style(b"safe-\x1b[2J\x1b[HINJECTED.txt"),
            "\"safe-\\033[2J\\033[HINJECTED.txt\""
        );
    }

    #[test]
    fn escapes_c1_control_bytes_like_git() {
        assert_eq!(
            quote_c_style(b"evil-\xc2\x9b2J\xc2\x9bH.txt"),
            "\"evil-\\302\\2332J\\302\\233H.txt\""
        );
    }

    #[test]
    fn escapes_del_and_high_bytes_faithfully() {
        assert_eq!(quote_c_style(b"a\x7fb\xff"), "\"a\\177b\\377\"");
    }

    #[test]
    fn uses_named_escapes_for_common_controls() {
        assert_eq!(quote_c_style(b"a\tb\nc\rd"), "\"a\\tb\\nc\\rd\"");
    }

    #[test]
    fn escapes_quotes_and_backslashes() {
        assert_eq!(quote_c_style(b"a\"b\\c"), "\"a\\\"b\\\\c\"");
    }
}

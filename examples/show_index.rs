use std::io::{self, Read};

use git_rs::{ShowIndexOptions, show_index};

fn main() -> git_rs::Result<()> {
    let options = ShowIndexOptions::default();
    let limit = u64::try_from(options.max_index_size)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut input = Vec::new();
    io::stdin().take(limit).read_to_end(&mut input)?;
    let report = show_index(&input, &options)?;
    for entry in report.entries {
        if let Some(crc) = entry.crc32 {
            println!("{} {} ({crc:08x})", entry.offset, entry.id);
        } else {
            println!("{} {}", entry.offset, entry.id);
        }
    }
    Ok(())
}

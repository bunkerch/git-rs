//! Git ignore pattern parsing and byte-oriented matching.

use std::path::{Component, Path};

use crate::{Error, Repository, Result};

/// One parsed ignore rule and the worktree directory containing its source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IgnoreRule {
    base: Vec<u8>,
    pattern: Vec<u8>,
    negated: bool,
    directory_only: bool,
    basename_only: bool,
}

impl IgnoreRule {
    #[must_use]
    pub fn pattern(&self) -> &[u8] {
        &self.pattern
    }

    #[must_use]
    pub const fn is_negated(&self) -> bool {
        self.negated
    }

    #[must_use]
    pub const fn is_directory_only(&self) -> bool {
        self.directory_only
    }
}

/// Ordered ignore rules. The final matching rule determines the result.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IgnoreMatcher {
    rules: Vec<IgnoreRule>,
}

impl IgnoreMatcher {
    #[must_use]
    pub fn rules(&self) -> &[IgnoreRule] {
        &self.rules
    }

    /// Append patterns whose paths are relative to `base`.
    ///
    /// Empty lines and unescaped comments are ignored. Invalid NUL-containing
    /// paths or patterns are rejected.
    ///
    /// # Errors
    /// Returns an error when the base or pattern file contains NUL.
    pub fn add_patterns(&mut self, base: &[u8], contents: &[u8]) -> Result<usize> {
        if base.contains(&0) || contents.contains(&0) {
            return Err(Error::InvalidRepository("ignore data contains NUL".into()));
        }
        let before = self.rules.len();
        for physical in contents.split(|byte| *byte == b'\n') {
            if let Some(rule) = parse_rule(base, physical) {
                self.rules.push(rule);
            }
        }
        Ok(self.rules.len() - before)
    }

    /// Determine whether `path` is ignored. Paths use index-style `/`
    /// separators and are relative to the worktree root.
    #[must_use]
    pub fn is_ignored(&self, path: &[u8], is_directory: bool) -> bool {
        let mut ignored = false;
        for rule in &self.rules {
            if rule.matches(path, is_directory) {
                ignored = !rule.negated;
            }
        }
        ignored
    }
}

impl IgnoreRule {
    fn matches(&self, path: &[u8], is_directory: bool) -> bool {
        let relative = if self.base.is_empty() {
            path
        } else if path.starts_with(&self.base) && path.get(self.base.len()) == Some(&b'/') {
            &path[self.base.len() + 1..]
        } else {
            return false;
        };
        if self.directory_only {
            let components = relative.split(|byte| *byte == b'/').collect::<Vec<_>>();
            let directory_components = components.len().saturating_sub(usize::from(!is_directory));
            if self.basename_only {
                return components
                    .iter()
                    .take(directory_components)
                    .any(|component| wildmatch(&self.pattern, component));
            }
            return relative
                .iter()
                .enumerate()
                .filter_map(|(index, byte)| (*byte == b'/').then_some(&relative[..index]))
                .chain(is_directory.then_some(relative))
                .any(|directory| wildmatch(&self.pattern, directory));
        }
        if self.basename_only {
            relative
                .split(|byte| *byte == b'/')
                .any(|component| wildmatch(&self.pattern, component))
        } else {
            wildmatch(&self.pattern, relative)
        }
    }
}

impl Repository {
    /// Load `info/exclude` and every applicable `.gitignore`, then test a path.
    ///
    /// This intentionally does not read a host-global excludes file: custom
    /// filesystem repositories must not silently cross their storage boundary.
    ///
    /// # Errors
    /// Returns an error for unsafe paths, bare repositories, malformed ignore
    /// files, or storage failures.
    pub fn is_ignored(&self, path: impl AsRef<Path>, is_directory: bool) -> Result<bool> {
        let work_tree = self.work_tree().ok_or_else(|| {
            Error::InvalidRepository("ignore matching requires a worktree".into())
        })?;
        let path = normalized_index_path(path.as_ref())?;
        let mut matcher = self.ignore_matcher()?;
        matcher.add_worktree_patterns(self, work_tree, b"")?;
        let mut base = Vec::new();
        let components = path.split(|byte| *byte == b'/').collect::<Vec<_>>();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            if !base.is_empty() {
                base.push(b'/');
            }
            base.extend_from_slice(component);
            matcher.add_worktree_patterns(self, work_tree, &base)?;
            if matcher.is_ignored(&base, true) {
                return Ok(true);
            }
        }
        Ok(matcher.is_ignored(&path, is_directory))
    }

    pub(crate) fn ignore_matcher(&self) -> Result<IgnoreMatcher> {
        let mut matcher = IgnoreMatcher::default();
        match self
            .filesystem()
            .read(&self.common_dir().join("info/exclude"))
        {
            Ok(contents) => {
                matcher.add_patterns(b"", &contents)?;
            }
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        Ok(matcher)
    }
}

impl IgnoreMatcher {
    pub(crate) fn add_worktree_patterns(
        &mut self,
        repository: &Repository,
        work_tree: &Path,
        base: &[u8],
    ) -> Result<()> {
        let relative = index_bytes_to_path(base)?;
        let ignore = work_tree.join(relative).join(".gitignore");
        match repository.filesystem().read(&ignore) {
            Ok(contents) => {
                self.add_patterns(base, &contents)?;
            }
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        Ok(())
    }
}

fn parse_rule(base: &[u8], physical: &[u8]) -> Option<IgnoreRule> {
    let mut line = physical.strip_suffix(b"\r").unwrap_or(physical).to_vec();
    trim_unescaped_spaces(&mut line);
    if line.is_empty() || line[0] == b'#' {
        return None;
    }
    if line.starts_with(br"\#") || line.starts_with(br"\!") {
        line.remove(0);
    }
    let negated = line.first() == Some(&b'!');
    if negated {
        line.remove(0);
    }
    if line.is_empty() {
        return None;
    }
    let directory_only = line.last() == Some(&b'/') && !escaped_at(&line, line.len() - 1);
    if directory_only {
        line.pop();
    }
    let anchored = line.first() == Some(&b'/');
    if anchored {
        line.remove(0);
    }
    if line.is_empty() {
        return None;
    }
    let basename_only = !anchored && !line.contains(&b'/');
    Some(IgnoreRule {
        base: base.to_vec(),
        pattern: line,
        negated,
        directory_only,
        basename_only,
    })
}

fn trim_unescaped_spaces(line: &mut Vec<u8>) {
    while line.last() == Some(&b' ') && !escaped_at(line, line.len() - 1) {
        line.pop();
    }
}

fn escaped_at(value: &[u8], index: usize) -> bool {
    let preceding = value[..index]
        .iter()
        .rev()
        .take_while(|byte| **byte == b'\\')
        .count();
    preceding % 2 == 1
}

pub(crate) fn wildmatch(pattern: &[u8], text: &[u8]) -> bool {
    wildmatch_mode(pattern, text, true)
}

pub(crate) fn wildmatch_ref(pattern: &[u8], text: &[u8]) -> bool {
    wildmatch_mode(pattern, text, false)
}

fn wildmatch_mode(pattern: &[u8], text: &[u8], slash_sensitive: bool) -> bool {
    let width = text.len() + 1;
    let Some(states) = pattern
        .len()
        .checked_add(1)
        .and_then(|rows| rows.checked_mul(width))
    else {
        return false;
    };
    let mut memo = Vec::new();
    if memo.try_reserve_exact(states).is_err() {
        return false;
    }
    memo.resize(states, None);
    wildmatch_at(pattern, text, 0, 0, width, slash_sensitive, &mut memo)
}

#[allow(clippy::too_many_lines)]
fn wildmatch_at(
    pattern: &[u8],
    text: &[u8],
    pattern_at: usize,
    text_at: usize,
    width: usize,
    slash_sensitive: bool,
    memo: &mut [Option<bool>],
) -> bool {
    let slot = pattern_at * width + text_at;
    if let Some(result) = memo[slot] {
        return result;
    }
    let result = if pattern_at == pattern.len() {
        text_at == text.len()
    } else {
        match pattern[pattern_at] {
            b'\\' => {
                let next = pattern_at + 1;
                next < pattern.len()
                    && text.get(text_at) == Some(&pattern[next])
                    && wildmatch_at(
                        pattern,
                        text,
                        next + 1,
                        text_at + 1,
                        width,
                        slash_sensitive,
                        memo,
                    )
            }
            b'?' => {
                text.get(text_at)
                    .is_some_and(|byte| !slash_sensitive || *byte != b'/')
                    && wildmatch_at(
                        pattern,
                        text,
                        pattern_at + 1,
                        text_at + 1,
                        width,
                        slash_sensitive,
                        memo,
                    )
            }
            b'*' => {
                let mut end = pattern_at + 1;
                while pattern.get(end) == Some(&b'*') {
                    end += 1;
                }
                let starstar = end - pattern_at > 1;
                if starstar && pattern.get(end) == Some(&b'/') {
                    wildmatch_at(
                        pattern,
                        text,
                        end + 1,
                        text_at,
                        width,
                        slash_sensitive,
                        memo,
                    ) || text.get(text_at).is_some_and(|byte| *byte != b'/')
                        && wildmatch_at(
                            pattern,
                            text,
                            pattern_at,
                            text_at + 1,
                            width,
                            slash_sensitive,
                            memo,
                        )
                        || text.get(text_at) == Some(&b'/')
                            && wildmatch_at(
                                pattern,
                                text,
                                pattern_at,
                                text_at + 1,
                                width,
                                slash_sensitive,
                                memo,
                            )
                } else {
                    wildmatch_at(pattern, text, end, text_at, width, slash_sensitive, memo)
                        || text.get(text_at).is_some()
                            && (starstar || !slash_sensitive || text[text_at] != b'/')
                            && wildmatch_at(
                                pattern,
                                text,
                                pattern_at,
                                text_at + 1,
                                width,
                                slash_sensitive,
                                memo,
                            )
                }
            }
            b'[' => match_class(
                pattern,
                text.get(text_at).copied(),
                pattern_at,
                slash_sensitive,
            )
            .is_some_and(|(matched, next)| {
                matched
                    && wildmatch_at(
                        pattern,
                        text,
                        next,
                        text_at + 1,
                        width,
                        slash_sensitive,
                        memo,
                    )
            }),
            literal => {
                text.get(text_at) == Some(&literal)
                    && wildmatch_at(
                        pattern,
                        text,
                        pattern_at + 1,
                        text_at + 1,
                        width,
                        slash_sensitive,
                        memo,
                    )
            }
        }
    };
    memo[slot] = Some(result);
    result
}

fn match_class(
    pattern: &[u8],
    value: Option<u8>,
    start: usize,
    slash_sensitive: bool,
) -> Option<(bool, usize)> {
    let value = value?;
    if slash_sensitive && value == b'/' {
        return Some((false, start + 1));
    }
    let mut at = start + 1;
    let negated = matches!(pattern.get(at), Some(b'!' | b'^'));
    at += usize::from(negated);
    let mut matched = false;
    let mut previous = None;
    while let Some(&current) = pattern.get(at) {
        if current == b']' && at > start + 1 + usize::from(negated) {
            return Some((matched != negated, at + 1));
        }
        let (current, next) = if current == b'\\' {
            (*pattern.get(at + 1)?, at + 2)
        } else {
            (current, at + 1)
        };
        if current == b'-' && previous.is_some() && pattern.get(next) != Some(&b']') {
            let end = *pattern.get(next)?;
            matched |= previous.unwrap() <= value && value <= end;
            previous = None;
            at = next + 1;
        } else {
            matched |= current == value;
            previous = Some(current);
            at = next;
        }
    }
    None
}

#[cfg(unix)]
fn normalized_index_path(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let mut output = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                if !output.is_empty() {
                    output.push(b'/');
                }
                output.extend_from_slice(value.as_bytes());
            }
            Component::CurDir => {}
            _ => return Err(Error::InvalidPath(path.to_path_buf())),
        }
    }
    Ok(output)
}

#[cfg(not(unix))]
fn normalized_index_path(path: &Path) -> Result<Vec<u8>> {
    path.to_str()
        .map(|value| value.replace(std::path::MAIN_SEPARATOR, "/").into_bytes())
        .ok_or_else(|| Error::InvalidPath(path.to_path_buf()))
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn index_bytes_to_path(path: &[u8]) -> Result<std::path::PathBuf> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    Ok(path
        .split(|byte| *byte == b'/')
        .filter(|component| !component.is_empty())
        .map(OsStr::from_bytes)
        .collect())
}

#[cfg(not(unix))]
fn index_bytes_to_path(path: &[u8]) -> Result<std::path::PathBuf> {
    std::str::from_utf8(path)
        .map(|value| value.split('/').collect())
        .map_err(|_| Error::InvalidPath("non-UTF-8 ignore path".into()))
}

#[cfg(test)]
mod tests {
    use super::IgnoreMatcher;

    #[test]
    fn parses_ordered_negated_directory_and_escaped_rules() {
        let mut matcher = IgnoreMatcher::default();
        matcher
            .add_patterns(
                b"",
                b"# comment\n*.log\n!important.log\nbuild/\n\\#literal\nspace\\ \n",
            )
            .unwrap();
        assert!(matcher.is_ignored(b"deep/error.log", false));
        assert!(!matcher.is_ignored(b"important.log", false));
        assert!(matcher.is_ignored(b"build", true));
        assert!(!matcher.is_ignored(b"build", false));
        assert!(matcher.is_ignored(b"#literal", false));
        assert!(matcher.is_ignored(b"space ", false));
    }

    #[test]
    fn matches_anchored_slashes_starstar_classes_and_question_marks() {
        let mut matcher = IgnoreMatcher::default();
        matcher
            .add_patterns(b"", b"/root.txt\na/**/b?.[ch]\nfile[0-9]\n")
            .unwrap();
        assert!(matcher.is_ignored(b"root.txt", false));
        assert!(!matcher.is_ignored(b"deep/root.txt", false));
        assert!(matcher.is_ignored(b"a/b1.c", false));
        assert!(matcher.is_ignored(b"a/x/y/bz.h", false));
        assert!(matcher.is_ignored(b"deep/file7", false));
        assert!(!matcher.is_ignored(b"filex", false));
    }

    #[test]
    fn lower_directory_rules_override_parent_rules() {
        let mut matcher = IgnoreMatcher::default();
        matcher.add_patterns(b"", b"*.tmp\n").unwrap();
        matcher.add_patterns(b"generated", b"!keep.tmp\n").unwrap();
        assert!(matcher.is_ignored(b"generated/drop.tmp", false));
        assert!(!matcher.is_ignored(b"generated/keep.tmp", false));
    }

    #[test]
    fn directory_only_rules_apply_to_all_descendants() {
        let mut matcher = IgnoreMatcher::default();
        matcher.add_patterns(b"", b"build/\nfoo/bar/\n").unwrap();
        assert!(matcher.is_ignored(b"build", true));
        assert!(matcher.is_ignored(b"build/output.o", false));
        assert!(matcher.is_ignored(b"nested/build/output.o", false));
        assert!(matcher.is_ignored(b"foo/bar/result", false));
        assert!(!matcher.is_ignored(b"foo/other/result", false));
    }
}

//! Git attribute parsing, precedence, and repository lookup.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::ignore::wildmatch;
use crate::worktree::worktree_path;
use crate::{EntryMode, Error, ObjectId, ObjectKind, Repository, Result};

/// One of Git's four attribute states.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AttributeValue {
    Set,
    Unset,
    Value(String),
    Unspecified,
}

/// Where version-controlled `.gitattributes` files are read from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttributeSource {
    /// Prefer the worktree and fall back to the stage-zero index entry.
    WorktreeThenIndex,
    /// Read only stage-zero index entries, including in a bare repository.
    Index,
    /// Read `.gitattributes` blobs and object modes from an explicit tree.
    Tree(ObjectId),
}

/// Attribute query selection and resource bounds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckAttributesOptions {
    pub source: AttributeSource,
    /// Empty means return every attribute which is not unspecified.
    pub attributes: Vec<String>,
    pub max_files: usize,
    pub max_file_size: usize,
    pub max_rules: usize,
    pub max_macro_depth: usize,
    pub max_paths: usize,
    pub max_results: usize,
}

impl Default for CheckAttributesOptions {
    fn default() -> Self {
        Self {
            source: AttributeSource::WorktreeThenIndex,
            attributes: Vec::new(),
            max_files: 4096,
            max_file_size: 1024 * 1024,
            max_rules: 1_000_000,
            max_macro_depth: 64,
            max_paths: 1_000_000,
            max_results: 10_000_000,
        }
    }
}

/// One path/attribute/value result in caller path and attribute order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttributeResult {
    path: Vec<u8>,
    name: String,
    value: AttributeValue,
}

impl AttributeResult {
    #[must_use]
    pub fn path(&self) -> &[u8] {
        &self.path
    }
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
    #[must_use]
    pub const fn value(&self) -> &AttributeValue {
        &self.value
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Assignment {
    name: String,
    value: AttributeValue,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AttributeRule {
    base: Vec<u8>,
    pattern: Vec<u8>,
    basename_only: bool,
    assignments: Vec<Assignment>,
}

#[derive(Default)]
struct AttributeSet {
    rules: Vec<AttributeRule>,
    macros: HashMap<String, Vec<Assignment>>,
}

impl Repository {
    /// Resolve Git attributes for repository-relative byte paths.
    ///
    /// Sources are loaded independently for each path to preserve directory
    /// precedence. Host-global/system attributes are intentionally excluded:
    /// abstract filesystem repositories must not escape their storage boundary.
    /// `$GIT_DIR/info/attributes` remains the highest-precedence source.
    ///
    /// # Errors
    /// Returns an error for unsafe paths/names, malformed attribute data,
    /// non-blob index entries, unavailable worktrees, or exceeded bounds.
    pub fn check_attributes(
        &self,
        paths: &[Vec<u8>],
        options: &CheckAttributesOptions,
    ) -> Result<Vec<AttributeResult>> {
        validate_options(paths, options)?;
        if matches!(options.source, AttributeSource::WorktreeThenIndex)
            && self.work_tree().is_none()
        {
            return Err(Error::InvalidRepository(
                "worktree attribute lookup requires a worktree".into(),
            ));
        }
        let index = self.read_index()?;
        let indexed = index
            .entries()
            .iter()
            .filter(|entry| entry.stage() == 0)
            .map(|entry| (entry.path().to_vec(), (entry.id(), entry.mode())))
            .collect::<BTreeMap<_, _>>();
        let mut output = Vec::new();
        for path in paths {
            let mut attributes = AttributeSet::default();
            let mut files = 0usize;
            attributes.add_builtin_binary_macro();
            for base in parent_bases(path) {
                if let Some(contents) = self.read_attribute_source(
                    &base,
                    options.source,
                    &indexed,
                    options,
                    &mut files,
                )? {
                    attributes.add_file(&base, &contents, base.is_empty(), options)?;
                }
            }
            if let Some(contents) = read_bounded(
                self.filesystem()
                    .read(&self.common_dir().join("info/attributes")),
                options.max_file_size,
            )? {
                files = files.saturating_add(1);
                enforce_file_limit(files, options.max_files)?;
                attributes.add_file(b"", &contents, true, options)?;
            }
            let mut values = attributes.evaluate(path, options.max_macro_depth)?;
            if let Some(mode) =
                self.attribute_object_mode(path, options.source, &indexed, options.max_file_size)?
            {
                values.insert("builtin_objectmode".into(), AttributeValue::Value(mode));
            }
            if options.attributes.is_empty() {
                for (name, value) in values {
                    if value != AttributeValue::Unspecified {
                        output.push(AttributeResult {
                            path: path.clone(),
                            name,
                            value,
                        });
                    }
                }
            } else {
                for name in &options.attributes {
                    output.push(AttributeResult {
                        path: path.clone(),
                        name: name.clone(),
                        value: values
                            .get(name)
                            .cloned()
                            .unwrap_or(AttributeValue::Unspecified),
                    });
                }
            }
            if output.len() > options.max_results {
                return Err(Error::InvalidRepository(
                    "attribute result count exceeds limit".into(),
                ));
            }
        }
        Ok(output)
    }

    fn read_attribute_source(
        &self,
        base: &[u8],
        source: AttributeSource,
        indexed: &BTreeMap<Vec<u8>, (crate::ObjectId, u32)>,
        options: &CheckAttributesOptions,
        files: &mut usize,
    ) -> Result<Option<Vec<u8>>> {
        let attribute_path = if base.is_empty() {
            b".gitattributes".to_vec()
        } else {
            [base, b"/.gitattributes"].concat()
        };
        let contents = match source {
            AttributeSource::WorktreeThenIndex => {
                let work_tree = self.work_tree().expect("validated worktree");
                let path = work_tree.join(worktree_path(&attribute_path)?);
                match read_bounded(self.filesystem().read(&path), options.max_file_size)? {
                    Some(contents) => Some(contents),
                    None => {
                        self.read_index_attribute(&attribute_path, indexed, options.max_file_size)?
                    }
                }
            }
            AttributeSource::Index => {
                self.read_index_attribute(&attribute_path, indexed, options.max_file_size)?
            }
            AttributeSource::Tree(tree) => {
                self.read_tree_attribute(tree, &attribute_path, options.max_file_size)?
            }
        };
        if contents.is_some() {
            *files = files.saturating_add(1);
            enforce_file_limit(*files, options.max_files)?;
        }
        Ok(contents)
    }

    fn read_index_attribute(
        &self,
        path: &[u8],
        indexed: &BTreeMap<Vec<u8>, (crate::ObjectId, u32)>,
        max_size: usize,
    ) -> Result<Option<Vec<u8>>> {
        let Some((id, mode)) = indexed.get(path) else {
            return Ok(None);
        };
        if !matches!(mode, 0o100_644 | 0o100_755) {
            return Err(Error::InvalidRepository(format!(
                "{} is not a regular attribute file",
                String::from_utf8_lossy(path)
            )));
        }
        let object = self.read_object(*id, max_size)?;
        if object.kind() != ObjectKind::Blob {
            return Err(Error::InvalidObject(format!(
                "attribute source {id} is not a blob"
            )));
        }
        Ok(Some(object.data().to_vec()))
    }

    fn read_tree_attribute(
        &self,
        tree: ObjectId,
        path: &[u8],
        max_size: usize,
    ) -> Result<Option<Vec<u8>>> {
        let Some((id, mode)) = self.find_tree_entry(tree, path, max_size)? else {
            return Ok(None);
        };
        if !matches!(mode, EntryMode::Blob | EntryMode::BlobExecutable) {
            return Ok(None);
        }
        let object = self.read_object(id, max_size)?;
        if object.kind() != ObjectKind::Blob {
            return Err(Error::InvalidObject(format!(
                "attribute source {id} is not a blob"
            )));
        }
        Ok(Some(object.data().to_vec()))
    }

    fn attribute_object_mode(
        &self,
        path: &[u8],
        source: AttributeSource,
        indexed: &BTreeMap<Vec<u8>, (ObjectId, u32)>,
        max_size: usize,
    ) -> Result<Option<String>> {
        if let AttributeSource::Tree(tree) = source {
            return Ok(self
                .find_tree_entry(tree, path, max_size)?
                .map(|(_, mode)| String::from_utf8_lossy(mode.as_octal()).into_owned()));
        }
        Ok(indexed.get(path).map(|(_, mode)| format!("{mode:o}")))
    }

    fn find_tree_entry(
        &self,
        root: ObjectId,
        path: &[u8],
        max_size: usize,
    ) -> Result<Option<(ObjectId, EntryMode)>> {
        let mut current = root;
        let mut components = path.split(|byte| *byte == b'/').peekable();
        while let Some(component) = components.next() {
            let tree = self.read_tree(current, max_size)?;
            let Some(entry) = tree
                .entries()
                .iter()
                .find(|entry| entry.name() == component)
            else {
                return Ok(None);
            };
            if components.peek().is_none() {
                return Ok(Some((entry.id(), entry.mode())));
            }
            if entry.mode() != EntryMode::Tree {
                return Ok(None);
            }
            current = entry.id();
        }
        Ok(None)
    }
}

impl AttributeSet {
    fn add_builtin_binary_macro(&mut self) {
        self.macros.insert(
            "binary".into(),
            ["diff", "merge", "text"]
                .into_iter()
                .map(|name| Assignment {
                    name: name.into(),
                    value: AttributeValue::Unset,
                })
                .collect(),
        );
    }

    fn add_file(
        &mut self,
        base: &[u8],
        contents: &[u8],
        macros_allowed: bool,
        options: &CheckAttributesOptions,
    ) -> Result<()> {
        if contents.contains(&0) {
            return Err(Error::InvalidRepository(
                "attribute file contains NUL".into(),
            ));
        }
        for (line_number, physical) in contents.split(|byte| *byte == b'\n').enumerate() {
            let line = physical.strip_suffix(b"\r").unwrap_or(physical);
            let Some((pattern, states)) = split_pattern(line)? else {
                continue;
            };
            let assignments = parse_assignments(states, line_number + 1)?;
            if let Some(name) = pattern.strip_prefix(b"[attr]") {
                if !macros_allowed {
                    return Err(Error::InvalidRepository(format!(
                        "attribute macro is not allowed below the root at line {}",
                        line_number + 1
                    )));
                }
                let name = std::str::from_utf8(name).map_err(|_| {
                    Error::InvalidRepository("attribute macro name is not UTF-8".into())
                })?;
                validate_definition_name(name)?;
                self.macros.insert(name.to_owned(), assignments);
                if self.rules.len().saturating_add(self.macros.len()) > options.max_rules {
                    return Err(Error::InvalidRepository(
                        "attribute rule count exceeds limit".into(),
                    ));
                }
                continue;
            }
            if pattern.first() == Some(&b'!') {
                return Err(Error::InvalidRepository(format!(
                    "negative attribute pattern at line {}",
                    line_number + 1
                )));
            }
            if pattern.last() == Some(&b'/') {
                continue;
            }
            let mut pattern = pattern;
            if pattern.first() == Some(&b'/') {
                pattern.remove(0);
            }
            let basename_only = !pattern.contains(&b'/');
            self.rules.push(AttributeRule {
                base: base.to_vec(),
                pattern,
                basename_only,
                assignments,
            });
            if self.rules.len() > options.max_rules {
                return Err(Error::InvalidRepository(
                    "attribute rule count exceeds limit".into(),
                ));
            }
        }
        Ok(())
    }

    fn evaluate(
        &self,
        path: &[u8],
        max_macro_depth: usize,
    ) -> Result<BTreeMap<String, AttributeValue>> {
        let mut values = BTreeMap::new();
        for rule in &self.rules {
            if rule.matches(path) {
                let mut active = HashSet::new();
                apply_assignments(
                    &rule.assignments,
                    &self.macros,
                    &mut values,
                    &mut active,
                    0,
                    max_macro_depth,
                )?;
            }
        }
        Ok(values)
    }
}

impl AttributeRule {
    fn matches(&self, path: &[u8]) -> bool {
        let relative = if self.base.is_empty() {
            path
        } else if path.starts_with(&self.base) && path.get(self.base.len()) == Some(&b'/') {
            &path[self.base.len() + 1..]
        } else {
            return false;
        };
        if self.basename_only {
            relative
                .rsplit(|byte| *byte == b'/')
                .next()
                .is_some_and(|name| wildmatch(&self.pattern, name))
        } else {
            wildmatch(&self.pattern, relative)
        }
    }
}

fn apply_assignments(
    assignments: &[Assignment],
    macros: &HashMap<String, Vec<Assignment>>,
    values: &mut BTreeMap<String, AttributeValue>,
    active: &mut HashSet<String>,
    depth: usize,
    max_depth: usize,
) -> Result<()> {
    if depth > max_depth {
        return Err(Error::InvalidRepository(
            "attribute macro depth exceeds limit".into(),
        ));
    }
    for assignment in assignments {
        values.insert(assignment.name.clone(), assignment.value.clone());
        if assignment.value == AttributeValue::Set
            && let Some(expansion) = macros.get(&assignment.name)
        {
            if !active.insert(assignment.name.clone()) {
                return Err(Error::InvalidRepository("recursive attribute macro".into()));
            }
            apply_assignments(expansion, macros, values, active, depth + 1, max_depth)?;
            active.remove(&assignment.name);
        }
    }
    Ok(())
}

fn split_pattern(line: &[u8]) -> Result<Option<(Vec<u8>, &[u8])>> {
    let line = trim_ascii_start(line);
    if line.is_empty() || line[0] == b'#' {
        return Ok(None);
    }
    if line[0] == b'"' {
        let (pattern, consumed) = parse_c_quoted(line)?;
        return Ok(Some((pattern, trim_ascii_start(&line[consumed..]))));
    }
    let end = line
        .iter()
        .position(u8::is_ascii_whitespace)
        .unwrap_or(line.len());
    let mut pattern = line[..end].to_vec();
    if pattern.starts_with(br"\#") || pattern.starts_with(br"\!") {
        pattern.remove(0);
    }
    Ok(Some((pattern, trim_ascii_start(&line[end..]))))
}

fn parse_assignments(mut states: &[u8], line: usize) -> Result<Vec<Assignment>> {
    let mut output = Vec::new();
    while !states.is_empty() {
        let end = states
            .iter()
            .position(u8::is_ascii_whitespace)
            .unwrap_or(states.len());
        let token = &states[..end];
        states = trim_ascii_start(&states[end..]);
        let (prefix, body) = if matches!(token.first(), Some(b'-' | b'!')) {
            (token[0], &token[1..])
        } else {
            (0, token)
        };
        let equals = body.iter().position(|byte| *byte == b'=');
        let name_bytes = equals.map_or(body, |index| &body[..index]);
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| Error::InvalidRepository(format!("non-UTF-8 attribute at line {line}")))?;
        validate_definition_name(name)?;
        let value = match (prefix, equals) {
            (b'-', None) => AttributeValue::Unset,
            (b'!', None) => AttributeValue::Unspecified,
            (0, None) => AttributeValue::Set,
            (0, Some(index)) => AttributeValue::Value(
                std::str::from_utf8(&body[index + 1..])
                    .map_err(|_| {
                        Error::InvalidRepository(format!("non-UTF-8 value at line {line}"))
                    })?
                    .to_owned(),
            ),
            _ => {
                return Err(Error::InvalidRepository(format!(
                    "invalid attribute state at line {line}"
                )));
            }
        };
        output.push(Assignment {
            name: name.to_owned(),
            value,
        });
    }
    Ok(output)
}

fn parse_c_quoted(line: &[u8]) -> Result<(Vec<u8>, usize)> {
    let mut output = Vec::new();
    let mut index = 1;
    while index < line.len() {
        match line[index] {
            b'"' => return Ok((output, index + 1)),
            b'\\' if index + 1 < line.len() => {
                index += 1;
                match line[index] {
                    b'n' => output.push(b'\n'),
                    b't' => output.push(b'\t'),
                    b'b' => output.push(8),
                    b'r' => output.push(b'\r'),
                    b'"' | b'\\' => output.push(line[index]),
                    byte @ b'0'..=b'7' => {
                        let mut value = byte - b'0';
                        for _ in 0..2 {
                            if line
                                .get(index + 1)
                                .is_some_and(|b| matches!(b, b'0'..=b'7'))
                            {
                                index += 1;
                                value = value.saturating_mul(8).saturating_add(line[index] - b'0');
                            }
                        }
                        output.push(value);
                    }
                    _ => {
                        return Err(Error::InvalidRepository(
                            "invalid quoted attribute escape".into(),
                        ));
                    }
                }
            }
            byte => output.push(byte),
        }
        index += 1;
    }
    Err(Error::InvalidRepository(
        "unterminated quoted attribute pattern".into(),
    ))
}

fn parent_bases(path: &[u8]) -> Vec<Vec<u8>> {
    let mut bases = vec![Vec::new()];
    for (index, byte) in path.iter().enumerate() {
        if *byte == b'/' && index > 0 {
            bases.push(path[..index].to_vec());
        }
    }
    bases
}

fn validate_options(paths: &[Vec<u8>], options: &CheckAttributesOptions) -> Result<()> {
    if paths.len() > options.max_paths {
        return Err(Error::InvalidRepository(
            "attribute path count exceeds limit".into(),
        ));
    }
    for path in paths {
        worktree_path(path)?;
        if path.is_empty() {
            return Err(Error::InvalidRepository("empty attribute path".into()));
        }
    }
    for name in &options.attributes {
        validate_query_name(name)?;
    }
    Ok(())
}

fn validate_query_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('-')
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
    {
        return Err(Error::InvalidRepository(format!(
            "invalid attribute name `{name}`"
        )));
    }
    Ok(())
}

fn validate_definition_name(name: &str) -> Result<()> {
    validate_query_name(name)?;
    if name.starts_with("builtin_") {
        return Err(Error::InvalidRepository(format!(
            "reserved attribute name `{name}`"
        )));
    }
    Ok(())
}

fn read_bounded(result: Result<Vec<u8>>, max_size: usize) -> Result<Option<Vec<u8>>> {
    match result {
        Ok(contents) if contents.len() <= max_size => Ok(Some(contents)),
        Ok(_) => Err(Error::InvalidRepository(
            "attribute file exceeds size limit".into(),
        )),
        Err(Error::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

fn enforce_file_limit(count: usize, maximum: usize) -> Result<()> {
    if count > maximum {
        Err(Error::InvalidRepository(
            "attribute source count exceeds limit".into(),
        ))
    } else {
        Ok(())
    }
}

fn trim_ascii_start(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        FileSystem, Index, IndexEntry, InitOptions, MemoryFileSystem, StatData, Tree, TreeEntry,
    };
    use std::path::Path;

    #[test]
    fn precedence_values_macros_and_patterns_match_git() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        fs.write(
            Path::new("repo/.gitattributes"),
            b"*.txt text diff=root\n[attr]media -text diff=media\nassets/** media\n\"quoted name.txt\" label=value\n",
        ).unwrap();
        fs.create_dir_all(Path::new("repo/src")).unwrap();
        fs.write(
            Path::new("repo/src/.gitattributes"),
            b"*.txt -text !diff local\n",
        )
        .unwrap();
        fs.write(
            Path::new("repo/.git/info/attributes"),
            b"src/special.txt diff=info\n",
        )
        .unwrap();
        let names = ["text", "diff", "local", "media", "label"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let results = repository
            .check_attributes(
                &[
                    b"src/special.txt".to_vec(),
                    b"assets/a.bin".to_vec(),
                    b"quoted name.txt".to_vec(),
                ],
                &CheckAttributesOptions {
                    attributes: names,
                    ..CheckAttributesOptions::default()
                },
            )
            .unwrap();
        let values = |path: &[u8]| {
            results
                .iter()
                .filter(|item| item.path() == path)
                .map(|item| (item.name(), item.value()))
                .collect::<BTreeMap<_, _>>()
        };
        let special = values(b"src/special.txt");
        assert_eq!(special["text"], &AttributeValue::Unset);
        assert_eq!(special["diff"], &AttributeValue::Value("info".into()));
        assert_eq!(special["local"], &AttributeValue::Set);
        let media = values(b"assets/a.bin");
        assert_eq!(media["media"], &AttributeValue::Set);
        assert_eq!(media["text"], &AttributeValue::Unset);
        assert_eq!(media["diff"], &AttributeValue::Value("media".into()));
        assert_eq!(
            values(b"quoted name.txt")["label"],
            &AttributeValue::Value("value".into())
        );
    }

    #[test]
    fn worktree_falls_back_to_index_and_cached_ignores_worktree() {
        let fs = MemoryFileSystem::new();
        let repository = Repository::init(fs.clone(), "repo", &InitOptions::default()).unwrap();
        let blob = repository
            .write_object(ObjectKind::Blob, b"*.dat indexed\n")
            .unwrap();
        let data = repository.write_object(ObjectKind::Blob, b"data").unwrap();
        let index = Index::default()
            .with_entries(vec![
                IndexEntry::new(
                    b".gitattributes".to_vec(),
                    0o100_644,
                    blob,
                    StatData::default(),
                )
                .unwrap(),
                IndexEntry::new(b"x.dat".to_vec(), 0o100_644, data, StatData::default()).unwrap(),
            ])
            .unwrap();
        repository.write_index(&index).unwrap();
        let query = |source| {
            repository
                .check_attributes(
                    &[b"x.dat".to_vec()],
                    &CheckAttributesOptions {
                        source,
                        attributes: vec![
                            "indexed".into(),
                            "live".into(),
                            "builtin_objectmode".into(),
                        ],
                        ..CheckAttributesOptions::default()
                    },
                )
                .unwrap()
        };
        assert_eq!(
            query(AttributeSource::WorktreeThenIndex)[0].value(),
            &AttributeValue::Set
        );
        fs.write(Path::new("repo/.gitattributes"), b"*.dat live\n")
            .unwrap();
        let live = query(AttributeSource::WorktreeThenIndex);
        assert_eq!(live[0].value(), &AttributeValue::Unspecified);
        assert_eq!(live[1].value(), &AttributeValue::Set);
        assert_eq!(
            query(AttributeSource::Index)[0].value(),
            &AttributeValue::Set
        );
        assert_eq!(
            query(AttributeSource::Index)[2].value(),
            &AttributeValue::Value("100644".into())
        );
    }

    #[test]
    fn explicit_tree_source_works_without_a_worktree_or_index() {
        let repository = Repository::init(
            MemoryFileSystem::new(),
            "repo.git",
            &InitOptions {
                bare: true,
                ..InitOptions::default()
            },
        )
        .unwrap();
        let attributes = repository
            .write_object(ObjectKind::Blob, b"*.bin binary custom=tree\n")
            .unwrap();
        let data = repository.write_object(ObjectKind::Blob, b"data").unwrap();
        let tree = repository
            .write_tree(
                &Tree::new(vec![
                    TreeEntry::new(EntryMode::Blob, b".gitattributes", attributes).unwrap(),
                    TreeEntry::new(EntryMode::BlobExecutable, b"tool.bin", data).unwrap(),
                ])
                .unwrap(),
            )
            .unwrap();
        let results = repository
            .check_attributes(
                &[b"tool.bin".to_vec()],
                &CheckAttributesOptions {
                    source: AttributeSource::Tree(tree),
                    attributes: vec![
                        "binary".into(),
                        "text".into(),
                        "custom".into(),
                        "builtin_objectmode".into(),
                    ],
                    ..CheckAttributesOptions::default()
                },
            )
            .unwrap();
        assert_eq!(results[0].value(), &AttributeValue::Set);
        assert_eq!(results[1].value(), &AttributeValue::Unset);
        assert_eq!(results[2].value(), &AttributeValue::Value("tree".into()));
        assert_eq!(results[3].value(), &AttributeValue::Value("100755".into()));
    }
}

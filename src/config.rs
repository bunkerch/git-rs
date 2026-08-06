//! Git configuration parsing, typed lookup, and canonical serialization.

use std::path::Path;

use crate::{Error, Repository, Result};

/// One normalized config assignment. Section and variable names are
/// case-insensitive; subsection and value bytes remain case-sensitive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigEntry {
    section: String,
    subsection: Option<Vec<u8>>,
    name: String,
    value: Option<Vec<u8>>,
}

impl ConfigEntry {
    #[must_use]
    pub fn section(&self) -> &str {
        &self.section
    }

    #[must_use]
    pub fn subsection(&self) -> Option<&[u8]> {
        self.subsection.as_deref()
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `None` represents Git's implicit boolean-true syntax (`name` without
    /// `= value`).
    #[must_use]
    pub fn value(&self) -> Option<&[u8]> {
        self.value.as_deref()
    }
}

/// Ordered multi-value Git configuration.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Config {
    entries: Vec<ConfigEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Key {
    section: String,
    subsection: Option<Vec<u8>>,
    name: String,
}

impl Config {
    /// Parse Git config bytes, including quoted subsections/values, comments,
    /// escapes, implicit booleans, and backslash line continuations.
    ///
    /// # Errors
    /// Returns an error with the physical line number for malformed sections,
    /// variables, quotes, escapes, NULs, or assignments outside a section.
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.contains(&0) {
            return config_error(1, "config contains NUL");
        }
        let logical = logical_lines(data)?;
        let mut section: Option<(String, Option<Vec<u8>>)> = None;
        let mut entries = Vec::new();
        for (line_number, line) in logical {
            let line = trim_ascii(&line);
            if line.is_empty() || matches!(line[0], b'#' | b';') {
                continue;
            }
            if line[0] == b'[' {
                section = Some(parse_section(trim_ascii(strip_comment(line)), line_number)?);
                continue;
            }
            let (current, subsection) = section.as_ref().ok_or_else(|| {
                Error::InvalidRepository(format!(
                    "config line {line_number}: variable outside a section"
                ))
            })?;
            let (name, value) = parse_assignment(line, line_number)?;
            entries.push(ConfigEntry {
                section: current.clone(),
                subsection: subsection.clone(),
                name,
                value,
            });
        }
        Ok(Self { entries })
    }

    #[must_use]
    pub fn entries(&self) -> &[ConfigEntry] {
        &self.entries
    }

    /// Return the final assignment for a canonical dotted key.
    ///
    /// # Errors
    /// Returns an error for malformed key syntax.
    pub fn get(&self, key: &str) -> Result<Option<&ConfigEntry>> {
        let key = parse_key(key)?;
        Ok(self.entries.iter().rev().find(|entry| key.matches(entry)))
    }

    /// Return every assignment in file order.
    ///
    /// # Errors
    /// Returns an error for malformed key syntax.
    pub fn get_all(&self, key: &str) -> Result<Vec<&ConfigEntry>> {
        let key = parse_key(key)?;
        Ok(self
            .entries
            .iter()
            .filter(|entry| key.matches(entry))
            .collect())
    }

    /// Interpret the final value using Git's boolean spellings. An implicit
    /// value is true and an explicit empty value is false.
    ///
    /// # Errors
    /// Returns an error for a missing key, malformed key, non-UTF-8 value, or
    /// unknown boolean spelling.
    pub fn get_bool(&self, key: &str) -> Result<bool> {
        let entry = self
            .get(key)?
            .ok_or_else(|| Error::InvalidRepository(format!("missing config key `{key}`")))?;
        let Some(value) = entry.value() else {
            return Ok(true);
        };
        let value = std::str::from_utf8(value)
            .map_err(|_| Error::InvalidRepository(format!("non-UTF-8 boolean `{key}`")))?;
        match value.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Ok(true),
            "" | "false" | "no" | "off" | "0" => Ok(false),
            _ => Err(Error::InvalidRepository(format!(
                "invalid boolean value for `{key}`"
            ))),
        }
    }

    /// Parse a signed integer with Git's binary `k`, `m`, or `g` suffixes.
    ///
    /// # Errors
    /// Returns an error for a missing/implicit/non-UTF-8/malformed/overflowing
    /// value.
    pub fn get_i64(&self, key: &str) -> Result<i64> {
        let value = self
            .get(key)?
            .and_then(ConfigEntry::value)
            .ok_or_else(|| Error::InvalidRepository(format!("missing integer `{key}`")))?;
        let value = std::str::from_utf8(value)
            .map_err(|_| Error::InvalidRepository(format!("non-UTF-8 integer `{key}`")))?
            .trim();
        let (number, multiplier) = match value.as_bytes().last().copied() {
            Some(b'k' | b'K') => (&value[..value.len() - 1], 1024i64),
            Some(b'm' | b'M') => (&value[..value.len() - 1], 1024i64.pow(2)),
            Some(b'g' | b'G') => (&value[..value.len() - 1], 1024i64.pow(3)),
            _ => (value, 1),
        };
        number
            .parse::<i64>()
            .ok()
            .and_then(|number| number.checked_mul(multiplier))
            .ok_or_else(|| Error::InvalidRepository(format!("invalid integer `{key}`")))
    }

    /// Replace every value for `key` with one explicit value.
    ///
    /// # Errors
    /// Returns an error for malformed keys or NUL-containing values.
    pub fn set(&mut self, key: &str, value: impl AsRef<[u8]>) -> Result<()> {
        let key = parse_key(key)?;
        let value = checked_value(value.as_ref())?;
        self.entries.retain(|entry| !key.matches(entry));
        self.entries.push(key.entry(Some(value)));
        Ok(())
    }

    /// Append an additional explicit value for `key`.
    ///
    /// # Errors
    /// Returns an error for malformed keys or NUL-containing values.
    pub fn add(&mut self, key: &str, value: impl AsRef<[u8]>) -> Result<()> {
        let key = parse_key(key)?;
        self.entries
            .push(key.entry(Some(checked_value(value.as_ref())?)));
        Ok(())
    }

    /// Replace every value with an implicit boolean-true assignment.
    ///
    /// # Errors
    /// Returns an error for malformed key syntax.
    pub fn set_implicit(&mut self, key: &str) -> Result<()> {
        let key = parse_key(key)?;
        self.entries.retain(|entry| !key.matches(entry));
        self.entries.push(key.entry(None));
        Ok(())
    }

    /// Remove all assignments and return their count.
    ///
    /// # Errors
    /// Returns an error for malformed key syntax.
    pub fn unset(&mut self, key: &str) -> Result<usize> {
        let key = parse_key(key)?;
        let before = self.entries.len();
        self.entries.retain(|entry| !key.matches(entry));
        Ok(before - self.entries.len())
    }

    /// Remove every entry in one exact subsection and return its count.
    /// Section names are case-insensitive; subsection bytes are case-sensitive.
    ///
    /// # Errors
    /// Returns an error for an invalid section name or NUL-containing subsection.
    pub fn remove_subsection(&mut self, section: &str, subsection: &[u8]) -> Result<usize> {
        let section = normalized_section(section)?;
        checked_value(subsection)?;
        let before = self.entries.len();
        self.entries.retain(|entry| {
            entry.section != section || entry.subsection.as_deref() != Some(subsection)
        });
        Ok(before - self.entries.len())
    }

    /// Rename an exact subsection in place and return the number of entries.
    ///
    /// # Errors
    /// Returns an error for an invalid section, NUL-containing subsection, or
    /// an already existing destination subsection.
    pub fn rename_subsection(&mut self, section: &str, old: &[u8], new: &[u8]) -> Result<usize> {
        let section = normalized_section(section)?;
        checked_value(old)?;
        let new = checked_value(new)?;
        if self.entries.iter().any(|entry| {
            entry.section == section && entry.subsection.as_deref() == Some(new.as_slice())
        }) {
            return Err(Error::InvalidRepository(
                "destination config subsection already exists".into(),
            ));
        }
        let mut changed = 0;
        for entry in &mut self.entries {
            if entry.section == section && entry.subsection.as_deref() == Some(old) {
                entry.subsection = Some(new.clone());
                changed += 1;
            }
        }
        Ok(changed)
    }

    /// Remove one exact variable from an exact subsection.
    ///
    /// # Errors
    /// Returns an error for an invalid section/variable or NUL subsection.
    pub fn unset_in_subsection(
        &mut self,
        section: &str,
        subsection: &[u8],
        name: &str,
    ) -> Result<usize> {
        let section = normalized_section(section)?;
        let name = normalized_section(name)?;
        checked_value(subsection)?;
        let before = self.entries.len();
        self.entries.retain(|entry| {
            entry.section != section
                || entry.subsection.as_deref() != Some(subsection)
                || entry.name != name
        });
        Ok(before - self.entries.len())
    }

    /// Replace one exact variable in a byte-preserving subsection.
    ///
    /// # Errors
    /// Returns an error for invalid section/variable names or NUL bytes.
    pub fn set_in_subsection(
        &mut self,
        section: &str,
        subsection: &[u8],
        name: &str,
        value: impl AsRef<[u8]>,
    ) -> Result<()> {
        let section = normalized_section(section)?;
        let name = normalized_section(name)?;
        let subsection = checked_value(subsection)?;
        let value = checked_value(value.as_ref())?;
        self.entries.retain(|entry| {
            entry.section != section
                || entry.subsection.as_deref() != Some(subsection.as_slice())
                || entry.name != name
        });
        self.entries.push(ConfigEntry {
            section,
            subsection: Some(subsection),
            name,
            value: Some(value),
        });
        Ok(())
    }

    /// Encode a canonical config file preserving entry order and multiplicity.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut output = Vec::new();
        let mut previous: Option<(&str, Option<&[u8]>)> = None;
        for entry in &self.entries {
            let current = (entry.section.as_str(), entry.subsection.as_deref());
            if previous != Some(current) {
                output.push(b'[');
                output.extend_from_slice(entry.section.as_bytes());
                if let Some(subsection) = &entry.subsection {
                    output.extend_from_slice(b" \"");
                    encode_quoted(&mut output, subsection);
                    output.push(b'"');
                }
                output.extend_from_slice(b"]\n");
                previous = Some(current);
            }
            output.push(b'\t');
            output.extend_from_slice(entry.name.as_bytes());
            if let Some(value) = &entry.value {
                output.extend_from_slice(b" = \"");
                encode_quoted(&mut output, value);
                output.push(b'"');
            }
            output.push(b'\n');
        }
        output
    }
}

impl Key {
    fn matches(&self, entry: &ConfigEntry) -> bool {
        self.section == entry.section
            && self.subsection == entry.subsection
            && self.name == entry.name
    }

    fn entry(&self, value: Option<Vec<u8>>) -> ConfigEntry {
        ConfigEntry {
            section: self.section.clone(),
            subsection: self.subsection.clone(),
            name: self.name.clone(),
            value,
        }
    }
}

impl Repository {
    /// Parse the common repository config.
    ///
    /// # Errors
    /// Returns an error for storage or config syntax failures.
    pub fn read_config(&self) -> Result<Config> {
        Config::parse(&self.read_git_file("config")?)
    }

    /// Atomically replace the common repository config.
    ///
    /// # Errors
    /// Returns an error for lock contention or storage failure.
    pub fn write_config(&self, config: &Config) -> Result<()> {
        self.write_atomic(Path::new("config"), &config.encode())?;
        self.invalidate_replacements()
    }
}

fn parse_key(value: &str) -> Result<Key> {
    let parts = value.split('.').collect::<Vec<_>>();
    if parts.len() < 2 {
        return Err(Error::InvalidRepository(format!(
            "invalid config key `{value}`"
        )));
    }
    let section = parts[0].to_ascii_lowercase();
    let name = parts[parts.len() - 1].to_ascii_lowercase();
    if !valid_name(&section) || !valid_name(&name) {
        return Err(Error::InvalidRepository(format!(
            "invalid config key `{value}`"
        )));
    }
    let subsection = (parts.len() > 2).then(|| parts[1..parts.len() - 1].join(".").into_bytes());
    Ok(Key {
        section,
        subsection,
        name,
    })
}

fn logical_lines(data: &[u8]) -> Result<Vec<(usize, Vec<u8>)>> {
    let mut result = Vec::new();
    let mut logical = Vec::new();
    let mut start = 1;
    let mut continuing = false;
    for (index, physical) in data.split_inclusive(|byte| *byte == b'\n').enumerate() {
        let line_number = index + 1;
        let mut line = physical.strip_suffix(b"\n").unwrap_or(physical);
        if line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
        }
        if logical.is_empty() {
            start = line_number;
        }
        logical.extend_from_slice(line);
        if !logical.is_empty() && continuation_position(&logical) == Some(logical.len() - 1) {
            logical.pop();
            continuing = true;
            continue;
        }
        continuing = false;
        result.push((start, std::mem::take(&mut logical)));
    }
    if continuing {
        return config_error(start, "unterminated line continuation");
    }
    Ok(result)
}

fn continuation_position(line: &[u8]) -> Option<usize> {
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in line.iter().copied().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if byte == b'\\' {
            if index + 1 == line.len() {
                return Some(index);
            }
            escaped = true;
        } else if byte == b'"' {
            quoted = !quoted;
        } else if !quoted && matches!(byte, b'#' | b';') {
            return None;
        }
    }
    None
}

fn strip_comment(line: &[u8]) -> &[u8] {
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in line.iter().copied().enumerate() {
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            quoted = !quoted;
        } else if !quoted && matches!(byte, b'#' | b';') {
            return &line[..index];
        }
    }
    line
}

fn parse_section(line: &[u8], line_number: usize) -> Result<(String, Option<Vec<u8>>)> {
    if line.last() != Some(&b']') {
        return config_error(line_number, "unterminated section");
    }
    let inner = trim_ascii(&line[1..line.len() - 1]);
    if let Some(space) = inner.iter().position(u8::is_ascii_whitespace) {
        let section = lowercase_name(&inner[..space], line_number)?;
        let subsection = trim_ascii(&inner[space..]);
        if subsection.len() < 2 || subsection[0] != b'"' || subsection.last() != Some(&b'"') {
            return config_error(line_number, "invalid subsection quoting");
        }
        let subsection = parse_quoted(&subsection[1..subsection.len() - 1], line_number, true)?;
        Ok((section, Some(subsection)))
    } else if let Some(dot) = inner.iter().position(|byte| *byte == b'.') {
        let section = lowercase_name(&inner[..dot], line_number)?;
        if inner[dot + 1..].is_empty() {
            return config_error(line_number, "empty subsection");
        }
        Ok((section, Some(inner[dot + 1..].to_ascii_lowercase())))
    } else {
        Ok((lowercase_name(inner, line_number)?, None))
    }
}

fn parse_assignment(line: &[u8], line_number: usize) -> Result<(String, Option<Vec<u8>>)> {
    let separator = line.iter().position(|byte| *byte == b'=');
    let (name, value) = separator.map_or((line, None), |separator| {
        (&line[..separator], Some(&line[separator + 1..]))
    });
    let name = lowercase_name(trim_ascii(name), line_number)?;
    let value = value
        .map(|value| parse_value(value, line_number))
        .transpose()?;
    Ok((name, value))
}

fn parse_value(value: &[u8], line_number: usize) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut trailing_whitespace = 0;
    for byte in value.iter().copied().skip_while(u8::is_ascii_whitespace) {
        if escaped {
            output.push(match byte {
                b't' => b'\t',
                b'b' => 8,
                b'n' => b'\n',
                b'\\' | b'"' => byte,
                _ => return config_error(line_number, "invalid value escape"),
            });
            escaped = false;
            trailing_whitespace = 0;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            quoted = !quoted;
            trailing_whitespace = 0;
        } else if !quoted && matches!(byte, b'#' | b';') {
            break;
        } else {
            output.push(byte);
            if !quoted && byte.is_ascii_whitespace() {
                trailing_whitespace += 1;
            } else {
                trailing_whitespace = 0;
            }
        }
    }
    if escaped || quoted {
        return config_error(line_number, "unterminated quote or escape");
    }
    output.truncate(output.len() - trailing_whitespace);
    Ok(output)
}

fn parse_quoted(value: &[u8], line_number: usize, subsection: bool) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut escaped = false;
    for byte in value.iter().copied() {
        if escaped {
            if subsection {
                output.push(byte);
            } else {
                output.push(match byte {
                    b't' => b'\t',
                    b'b' => 8,
                    b'n' => b'\n',
                    b'\\' | b'"' => byte,
                    _ => return config_error(line_number, "invalid quoted escape"),
                });
            }
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            return config_error(line_number, "unescaped subsection quote");
        } else {
            output.push(byte);
        }
    }
    if escaped {
        return config_error(line_number, "unterminated subsection escape");
    }
    Ok(output)
}

fn encode_quoted(output: &mut Vec<u8>, value: &[u8]) {
    for byte in value {
        match byte {
            b'\n' => output.extend_from_slice(b"\\n"),
            b'\t' => output.extend_from_slice(b"\\t"),
            8 => output.extend_from_slice(b"\\b"),
            b'\\' => output.extend_from_slice(b"\\\\"),
            b'"' => output.extend_from_slice(b"\\\""),
            byte => output.push(*byte),
        }
    }
}

fn lowercase_name(value: &[u8], line_number: usize) -> Result<String> {
    let value = std::str::from_utf8(value)
        .map_err(|_| {
            Error::InvalidRepository(format!("config line {line_number}: non-UTF-8 name"))
        })?
        .to_ascii_lowercase();
    if !valid_name(&value) {
        return config_error(line_number, "invalid section or variable name");
    }
    Ok(value)
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn normalized_section(value: &str) -> Result<String> {
    let value = value.to_ascii_lowercase();
    if !valid_name(&value) {
        return Err(Error::InvalidRepository("invalid config name".into()));
    }
    Ok(value)
}

fn checked_value(value: &[u8]) -> Result<Vec<u8>> {
    if value.contains(&0) {
        return Err(Error::InvalidRepository("config value contains NUL".into()));
    }
    Ok(value.to_vec())
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

fn config_error<T>(line: usize, message: &str) -> Result<T> {
    Err(Error::InvalidRepository(format!(
        "config line {line}: {message}"
    )))
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn parses_git_syntax_and_typed_values() {
        let config = Config::parse(
            br#"
# leading comment
[Core]
	bare
	filemode = yes
	size = 2k
	quoted = "a\nb\t\"\\"
	continued = one\
 two
[remote "Origin.With.Dot"]
	url = "ssh://example/#repo" ; trailing comment
	fetch = one
	fetch = two
[section.OldSub]
	key = value
[commented "sub#section"] ; section comment
	key = ok
"#,
        )
        .unwrap();

        assert!(config.get_bool("core.bare").unwrap());
        assert!(config.get_bool("CORE.FILEMODE").unwrap());
        assert_eq!(config.get_i64("core.size").unwrap(), 2048);
        assert_eq!(
            config.get("core.quoted").unwrap().unwrap().value(),
            Some(b"a\nb\t\"\\".as_slice())
        );
        assert_eq!(
            config.get("core.continued").unwrap().unwrap().value(),
            Some(b"one two".as_slice())
        );
        assert_eq!(
            config
                .get_all("remote.Origin.With.Dot.fetch")
                .unwrap()
                .iter()
                .map(|entry| entry.value().unwrap())
                .collect::<Vec<_>>(),
            vec![b"one".as_slice(), b"two".as_slice()]
        );
        assert!(config.get("remote.origin.with.dot.url").unwrap().is_none());
        assert_eq!(
            config.get("section.oldsub.key").unwrap().unwrap().value(),
            Some(b"value".as_slice())
        );
        assert_eq!(
            config
                .get("commented.sub#section.key")
                .unwrap()
                .unwrap()
                .value(),
            Some(b"ok".as_slice())
        );
    }

    #[test]
    fn mutations_round_trip_bytes_and_multi_values() {
        let mut config = Config::default();
        config.set("core.bare", b"false").unwrap();
        config.set_implicit("feature.enabled").unwrap();
        config.add("remote.origin.fetch", b"one").unwrap();
        config
            .add("remote.origin.fetch", b"two # \"\\\n\t\x08")
            .unwrap();
        config.set("bytes.value", [0xff, b'\n', b'"']).unwrap();

        let encoded = config.encode();
        let mut decoded = Config::parse(&encoded).unwrap();
        assert_eq!(decoded, config);
        assert!(!decoded.get_bool("core.bare").unwrap());
        assert_eq!(decoded.unset("remote.origin.fetch").unwrap(), 2);
    }

    #[test]
    fn rejects_malformed_input() {
        for input in [
            b"key = value\n".as_slice(),
            b"[core\nkey = value\n",
            b"[core]\nkey = \\q\n",
            b"[core]\nkey = \"open\n",
            b"[core]\nkey = value\\",
            b"[core]\nbad_name = value\n",
            b"[core]\nkey = a\0b\n",
        ] {
            assert!(Config::parse(input).is_err(), "accepted {input:?}");
        }
    }
}

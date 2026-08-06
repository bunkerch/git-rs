//! Typed Git fetch and push refspecs.

use std::fmt;

use crate::{Error, ReferenceName, Result};

/// Validation rules for a refspec's direction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefSpecKind {
    Fetch,
    Push,
}

/// One parsed fetch or push refspec.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefSpec {
    kind: RefSpecKind,
    source: String,
    destination: Option<String>,
    flags: u8,
}

const FORCE: u8 = 1;
const NEGATIVE: u8 = 2;
const PATTERN: u8 = 4;
const MATCHING: u8 = 8;

impl RefSpec {
    /// Parse using Git's fetch-side rules.
    ///
    /// # Errors
    /// Returns an error for malformed flags, references, wildcard cardinality,
    /// negative destinations, or NUL/newline-containing input.
    pub fn parse_fetch(value: &str) -> Result<Self> {
        Self::parse(value, RefSpecKind::Fetch)
    }

    /// Parse using Git's push-side rules.
    ///
    /// # Errors
    /// Returns an error for malformed flags, references, wildcard cardinality,
    /// empty destinations, or NUL/newline-containing input.
    pub fn parse_push(value: &str) -> Result<Self> {
        Self::parse(value, RefSpecKind::Push)
    }

    #[must_use]
    pub const fn kind(&self) -> RefSpecKind {
        self.kind
    }

    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    #[must_use]
    pub fn destination(&self) -> Option<&str> {
        self.destination.as_deref()
    }

    #[must_use]
    pub const fn is_force(&self) -> bool {
        self.flags & FORCE != 0
    }

    #[must_use]
    pub const fn is_negative(&self) -> bool {
        self.flags & NEGATIVE != 0
    }

    #[must_use]
    pub const fn is_pattern(&self) -> bool {
        self.flags & PATTERN != 0
    }

    #[must_use]
    pub const fn is_matching(&self) -> bool {
        self.flags & MATCHING != 0
    }

    /// Test whether a source reference is selected. Negative refspecs use the
    /// same matcher and can therefore exclude an exact or wildcard source.
    #[must_use]
    pub fn matches(&self, source: &str) -> bool {
        if self.is_matching() {
            return source.starts_with("refs/heads/");
        }
        if self.is_pattern() {
            wildcard_capture(&self.source, source).is_some()
        } else {
            self.source == source || (self.source.is_empty() && source == "HEAD")
        }
    }

    /// Map a matching source ref through the destination wildcard.
    /// `None` means no match or an omitted/empty destination.
    #[must_use]
    pub fn map_destination(&self, source: &str) -> Option<String> {
        if self.is_negative() || self.is_matching() {
            return None;
        }
        let destination = self.destination.as_deref()?;
        if destination.is_empty() {
            return None;
        }
        if self.is_pattern() {
            let captured = wildcard_capture(&self.source, source)?;
            Some(destination.replacen('*', captured, 1))
        } else if self.matches(source) {
            Some(destination.to_owned())
        } else {
            None
        }
    }

    /// Test whether a local ref is selected by this destination side.
    #[must_use]
    pub fn matches_destination(&self, destination: &str) -> bool {
        let Some(pattern) = self.destination.as_deref() else {
            return false;
        };
        if self.is_pattern() {
            wildcard_capture(pattern, destination).is_some()
        } else {
            pattern == destination
        }
    }

    fn parse(value: &str, kind: RefSpecKind) -> Result<Self> {
        if value.contains(['\0', '\n', '\r']) {
            return refspec_error("refspec contains NUL or newline");
        }
        let (force, negative, body) = if let Some(body) = value.strip_prefix('+') {
            (true, false, body)
        } else if let Some(body) = value.strip_prefix('^') {
            (false, true, body)
        } else {
            (false, false, value)
        };
        if negative && kind == RefSpecKind::Push {
            return refspec_error("negative refspecs are fetch-only");
        }
        let separator = body.rfind(':');
        if negative && separator.is_some() {
            return refspec_error("negative refspec cannot have a destination");
        }
        if kind == RefSpecKind::Push && body == ":" {
            return Ok(Self {
                kind,
                source: String::new(),
                destination: Some(String::new()),
                flags: (u8::from(force) * FORCE) | (u8::from(negative) * NEGATIVE) | MATCHING,
            });
        }
        let (mut source, destination) = separator.map_or((body, None), |at| {
            (&body[..at], Some(body[at + 1..].to_owned()))
        });
        if source == "@" {
            source = "HEAD";
        }
        let source_stars = source.bytes().filter(|byte| *byte == b'*').count();
        let destination_stars = destination.as_deref().map_or(0, |value| {
            value.bytes().filter(|byte| *byte == b'*').count()
        });
        if source_stars > 1 || destination_stars > 1 {
            return refspec_error("refspec has more than one wildcard per side");
        }
        let pattern = source_stars == 1 || destination_stars == 1;
        if source_stars != destination_stars && destination.is_some() {
            return refspec_error("source and destination wildcards do not correspond");
        }
        if kind == RefSpecKind::Fetch {
            validate_fetch(source, destination.as_deref(), negative, pattern)?;
        } else {
            validate_push(source, destination.as_deref(), pattern)?;
        }
        Ok(Self {
            kind,
            source: source.to_owned(),
            destination,
            flags: (u8::from(force) * FORCE)
                | (u8::from(negative) * NEGATIVE)
                | (u8::from(pattern) * PATTERN),
        })
    }
}

impl fmt::Display for RefSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_force() {
            formatter.write_str("+")?;
        } else if self.is_negative() {
            formatter.write_str("^")?;
        }
        if self.is_matching() {
            return formatter.write_str(":");
        }
        formatter.write_str(&self.source)?;
        if let Some(destination) = &self.destination {
            formatter.write_str(":")?;
            formatter.write_str(destination)?;
        }
        Ok(())
    }
}

fn validate_fetch(
    source: &str,
    destination: Option<&str>,
    negative: bool,
    pattern: bool,
) -> Result<()> {
    if negative && source.is_empty() {
        return refspec_error("negative refspec source is empty");
    }
    if !source.is_empty()
        && !is_full_id(source)
        && source != "HEAD"
        && !valid_refspec_name(source, pattern)
    {
        return refspec_error("invalid fetch source");
    }
    if negative && is_full_id(source) {
        return refspec_error("negative refspec cannot select an object ID");
    }
    if pattern && destination.is_none() && !negative {
        return refspec_error("fetch wildcard requires a destination");
    }
    if let Some(destination) = destination
        && !destination.is_empty()
        && !valid_refspec_name(destination, pattern)
    {
        return refspec_error("invalid fetch destination");
    }
    Ok(())
}

fn validate_push(source: &str, destination: Option<&str>, pattern: bool) -> Result<()> {
    if pattern && !valid_refspec_name(source, true) {
        return refspec_error("invalid wildcard push source");
    }
    match destination {
        None if !valid_refspec_name(source, false) => {
            refspec_error("push without destination requires a reference source")
        }
        Some("") => refspec_error("push destination is empty"),
        Some(destination) if !valid_refspec_name(destination, pattern) => {
            refspec_error("invalid push destination")
        }
        _ => Ok(()),
    }
}

fn valid_refspec_name(value: &str, pattern: bool) -> bool {
    if value.is_empty() {
        return false;
    }
    let expanded = if pattern {
        value.replacen('*', "wildcard", 1)
    } else {
        value.to_owned()
    };
    if expanded.starts_with("refs/") {
        ReferenceName::new(expanded).is_ok()
    } else {
        !expanded.contains(['/', ' ', '\t', ':', '?', '[', '\\', '^', '~'])
            && !expanded.starts_with('.')
            && !expanded.ends_with('.')
            && !expanded.to_ascii_lowercase().ends_with(".lock")
    }
}

fn is_full_id(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn wildcard_capture<'a>(pattern: &str, value: &'a str) -> Option<&'a str> {
    let star = pattern.find('*')?;
    let (prefix, suffix_with_star) = pattern.split_at(star);
    let suffix = &suffix_with_star[1..];
    value
        .strip_prefix(prefix)
        .and_then(|value| value.strip_suffix(suffix))
}

fn refspec_error<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::InvalidReference(message.into()))
}

#[cfg(test)]
mod tests {
    use super::RefSpec;

    #[test]
    fn parses_formats_and_maps_fetch_refspecs() {
        let spec = RefSpec::parse_fetch("+refs/heads/*:refs/remotes/origin/*").unwrap();
        assert!(spec.is_force());
        assert!(spec.is_pattern());
        assert_eq!(spec.to_string(), "+refs/heads/*:refs/remotes/origin/*");
        assert_eq!(
            spec.map_destination("refs/heads/topic").as_deref(),
            Some("refs/remotes/origin/topic")
        );
        assert!(spec.map_destination("refs/tags/topic").is_none());

        let negative = RefSpec::parse_fetch("^refs/heads/private/*").unwrap();
        assert!(negative.is_negative());
        assert!(negative.matches("refs/heads/private/key"));
        assert_eq!(negative.to_string(), "^refs/heads/private/*");
    }

    #[test]
    fn parses_push_delete_matching_and_wildcard_forms() {
        let deletion = RefSpec::parse_push(":refs/heads/obsolete").unwrap();
        assert_eq!(deletion.source(), "");
        assert_eq!(deletion.destination(), Some("refs/heads/obsolete"));

        let matching = RefSpec::parse_push(":").unwrap();
        assert!(matching.is_matching());
        assert!(matching.matches("refs/heads/main"));
        assert!(!matching.matches("refs/tags/v1"));

        let wildcard = RefSpec::parse_push("refs/heads/*:refs/backup/*").unwrap();
        assert_eq!(
            wildcard.map_destination("refs/heads/main").as_deref(),
            Some("refs/backup/main")
        );
    }

    #[test]
    fn rejects_direction_and_wildcard_errors() {
        for value in [
            "^refs/heads/main:refs/remotes/origin/main",
            "refs/heads/*",
            "refs/heads/*:refs/remotes/origin/main",
            "refs/heads/**:refs/remotes/origin/*",
        ] {
            assert!(RefSpec::parse_fetch(value).is_err(), "accepted {value}");
        }
        for value in [
            "^refs/heads/main",
            "refs/heads/main:",
            "refs/heads/*:refs/backup/main",
        ] {
            assert!(RefSpec::parse_push(value).is_err(), "accepted {value}");
        }
    }
}

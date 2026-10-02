//! Scope: which files this tool is allowed to manage at all.
//!
//! Nothing here decides *where* a file goes — that is the policy's job. This module only
//! answers "may the engine touch this path?", and it is deliberately the outermost gate:
//! scope is checked before usage, size or any other consideration, so a file that is out
//! of scope can never be moved by a rule that later grows more eager.

use std::path::Path;

use globset::{Glob, GlobSet, GlobSetBuilder};
use thiserror::Error;

use crate::disk_management::FileEntry;

#[derive(Debug, Error)]
pub enum ScopeError {
    #[error("bad glob pattern {pattern:?}: {source}")]
    BadGlob {
        pattern: String,
        #[source]
        source: globset::Error,
    },
    #[error(
        "bad size {value:?}: expected a number with an optional binary suffix \
         (B, K/KiB, M/MiB, G/GiB, T/TiB), e.g. 512MiB or 2G"
    )]
    BadSize { value: String },
}

/// Why a path is not the engine's to move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejected {
    /// Outside the configured include set, or matched by an exclude pattern.
    OutOfScope,
    TooSmall {
        size: u64,
        min: u64,
    },
    TooLarge {
        size: u64,
        max: u64,
    },
}

/// The set of paths the engine may act on, plus the size window it will bother with.
///
/// Patterns are matched against a file's path *relative to the watched root*, and against
/// each of its ancestor directories — so `media/**` covers the whole subtree and
/// `--exclude node_modules` drops a directory anywhere in the tree. A bare pattern with no
/// separator and no wildcard (`node_modules`, `*.part`) is also expanded to match at any
/// depth, because that is what people mean by it.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    include: Option<GlobSet>,
    exclude: GlobSet,
    min_size: u64,
    max_size: Option<u64>,
}

impl Scope {
    /// Everything under the root is fair game, with no size bounds.
    pub fn everything() -> Self {
        Self::default()
    }

    pub fn build(
        include: &[String],
        exclude: &[String],
        min_size: u64,
        max_size: Option<u64>,
    ) -> Result<Self, ScopeError> {
        Ok(Self {
            include: if include.is_empty() {
                None
            } else {
                Some(build_set(include)?)
            },
            exclude: build_set(exclude)?,
            min_size,
            max_size,
        })
    }

    /// True when the size window cannot contain anything, i.e. min > max.
    pub fn is_empty_window(&self) -> bool {
        matches!(self.max_size, Some(max) if self.min_size > max)
    }

    /// May the engine touch this entry? `Err` carries the reason it may not.
    pub fn allows(&self, entry: &FileEntry) -> Result<(), Rejected> {
        let relative = entry.relative.as_path();

        if let Some(include) = &self.include {
            if !matches_path_or_ancestor(include, relative) {
                return Err(Rejected::OutOfScope);
            }
        }
        if matches_path_or_ancestor(&self.exclude, relative) {
            return Err(Rejected::OutOfScope);
        }
        if entry.size < self.min_size {
            return Err(Rejected::TooSmall {
                size: entry.size,
                min: self.min_size,
            });
        }
        if let Some(max) = self.max_size {
            if entry.size > max {
                return Err(Rejected::TooLarge {
                    size: entry.size,
                    max,
                });
            }
        }
        Ok(())
    }

    pub fn min_size(&self) -> u64 {
        self.min_size
    }

    pub fn max_size(&self) -> Option<u64> {
        self.max_size
    }
}

fn build_set(patterns: &[String]) -> Result<GlobSet, ScopeError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let mut variants = vec![pattern.clone()];
        // A bare name is meant "anywhere": expand it so `--exclude node_modules` and
        // `--exclude *.part` behave the way they read.
        if !pattern.contains('/') && !pattern.contains('*') && !pattern.contains('?') {
            variants.push(format!("**/{pattern}"));
            variants.push(format!("**/{pattern}/**"));
        }
        for variant in variants {
            let glob = Glob::new(&variant).map_err(|source| ScopeError::BadGlob {
                pattern: pattern.clone(),
                source,
            })?;
            builder.add(glob);
        }
    }
    builder.build().map_err(|source| ScopeError::BadGlob {
        pattern: patterns.join(", "),
        source,
    })
}

/// Match the path itself or any ancestor directory of it.
///
/// Ancestors matter for both directions: including a directory has to include what is
/// inside it, and excluding a directory has to exclude its contents.
fn matches_path_or_ancestor(set: &GlobSet, relative: &Path) -> bool {
    if set.is_empty() {
        return false;
    }
    relative
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .any(|ancestor| set.is_match(ancestor))
}

/// Parse a size like `512`, `64KiB`, `1.5G`, `2TiB`.
///
/// Every suffix is binary (K = 1024), with or without a trailing `B`, because that is what
/// people mean when sizing disks — `1G` is 1 GiB whether or not they write the `i`.
pub fn parse_size(text: &str) -> Result<u64, ScopeError> {
    let trimmed = text.trim();
    let bad = || ScopeError::BadSize {
        value: text.to_string(),
    };

    let split_at = trimmed
        .find(|ch: char| !ch.is_ascii_digit() && ch != '.' && ch != '_')
        .unwrap_or(trimmed.len());
    let (number, suffix) = trimmed.split_at(split_at);
    let number = number.replace('_', "");
    if number.is_empty() {
        return Err(bad());
    }
    let value: f64 = number.parse().map_err(|_| bad())?;
    if !value.is_finite() || value < 0.0 {
        return Err(bad());
    }

    let multiplier: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        _ => return Err(bad()),
    };

    Ok((value * multiplier as f64) as u64)
}

/// Sizes are printed in the same units they are configured in.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
        ("B", 1),
    ];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            return if scale == 1 {
                format!("{bytes} B")
            } else {
                format!("{:.1} {unit}", bytes as f64 / scale as f64)
            };
        }
    }
    "0 B".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn entry(relative: &str, size: u64) -> FileEntry {
        FileEntry {
            path: PathBuf::from("/watch").join(relative),
            relative: PathBuf::from(relative),
            size,
            last_access: SystemTime::now(),
            is_symlink: false,
        }
    }

    fn scope(include: &[&str], exclude: &[&str]) -> Scope {
        Scope::build(
            &include.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &exclude.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            0,
            None,
        )
        .unwrap()
    }

    #[test]
    fn everything_is_allowed_with_no_scope_configured() {
        let scope = Scope::everything();
        assert!(scope.allows(&entry("anything/deep/here.bin", 10)).is_ok());
    }

    #[test]
    fn include_covers_the_whole_subtree() {
        let scope = scope(&["media/**"], &[]);
        assert!(scope.allows(&entry("media/shows/s1/ep1.mkv", 10)).is_ok());
        assert_eq!(
            scope.allows(&entry("scratch/notes.txt", 10)),
            Err(Rejected::OutOfScope)
        );
    }

    #[test]
    fn a_bare_directory_pattern_matches_at_any_depth() {
        let scope = scope(&[], &["node_modules"]);
        assert_eq!(
            scope.allows(&entry("app/frontend/node_modules/react/index.js", 10)),
            Err(Rejected::OutOfScope)
        );
        assert!(scope
            .allows(&entry("app/frontend/src/index.js", 10))
            .is_ok());
    }

    #[test]
    fn excluding_a_directory_excludes_its_contents() {
        let scope = scope(&[], &["media/.git/**"]);
        assert_eq!(
            scope.allows(&entry("media/.git/objects/ab/cdef", 10)),
            Err(Rejected::OutOfScope)
        );
        assert!(scope.allows(&entry("media/.gitignore", 10)).is_ok());
    }

    #[test]
    fn size_bounds_are_inclusive_at_the_edges() {
        let scope = Scope::build(&[], &[], 1 << 20, Some(1 << 30)).unwrap();
        assert!(scope.allows(&entry("a.bin", 1 << 20)).is_ok());
        assert!(scope.allows(&entry("a.bin", 1 << 30)).is_ok());
        assert_eq!(
            scope.allows(&entry("a.bin", (1 << 20) - 1)),
            Err(Rejected::TooSmall {
                size: (1 << 20) - 1,
                min: 1 << 20
            })
        );
        assert_eq!(
            scope.allows(&entry("a.bin", (1 << 30) + 1)),
            Err(Rejected::TooLarge {
                size: (1 << 30) + 1,
                max: 1 << 30
            })
        );
    }

    #[test]
    fn scope_beats_size_bounds() {
        // A file both out of scope and too small reports the outermost reason.
        let scope = Scope::build(&["media/**".to_string()], &[], 1 << 30, None).unwrap();
        assert_eq!(
            scope.allows(&entry("scratch/tiny.txt", 4)),
            Err(Rejected::OutOfScope)
        );
    }

    #[test]
    fn sizes_parse_in_binary_units() {
        assert_eq!(parse_size("512").unwrap(), 512);
        assert_eq!(parse_size("1K").unwrap(), 1024);
        assert_eq!(parse_size("64KiB").unwrap(), 65_536);
        assert_eq!(parse_size("1.5M").unwrap(), 1_572_864);
        assert_eq!(parse_size("2G").unwrap(), 2 << 30);
        assert_eq!(parse_size("1tib").unwrap(), 1 << 40);
        assert!(parse_size("").is_err());
        assert!(parse_size("big").is_err());
        assert!(parse_size("10XB").is_err());
        assert!(parse_size("-5M").is_err());
    }

    #[test]
    fn empty_window_is_detectable() {
        let scope = Scope::build(&[], &[], 10, Some(5)).unwrap();
        assert!(scope.is_empty_window());
        assert!(!Scope::everything().is_empty_window());
    }
}

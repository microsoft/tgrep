//! Query-time visibility for a hidden-inclusive index.

use std::collections::BTreeMap;
use std::fs::Metadata;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::gitignore::IgnoreMatcher;

/// Hidden file/directory entries, relative to the index root. Retaining the
/// directory barriers (rather than marking all descendants) lets an explicitly
/// named hidden search root expose its otherwise visible children.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathVisibility {
    hidden: BTreeMap<String, bool>,
}

impl PathVisibility {
    pub fn is_empty(&self) -> bool {
        self.hidden.is_empty()
    }

    /// Record an entry using metadata already obtained by a walk or watcher.
    /// On Windows the walker caches directory-entry attributes, so this adds no
    /// per-candidate filesystem queries to an indexed search.
    pub fn record(
        &mut self,
        relative: &str,
        is_dir: bool,
        metadata: Option<&Metadata>,
        ignore: Option<&IgnoreMatcher>,
    ) -> bool {
        if relative.is_empty() {
            return false;
        }
        let path = Path::new(relative);
        let hidden = is_hidden(path, metadata)
            && !ignore.is_some_and(|matcher| matcher.is_whitelisted(path, is_dir));
        if hidden {
            self.hidden.insert(relative.to_string(), is_dir) != Some(is_dir)
        } else {
            self.hidden.remove(relative).is_some()
        }
    }

    /// Ignore-file whitelists can admit a hidden entry even without --hidden.
    pub fn apply_ignore_rules(&mut self, ignore: Option<&IgnoreMatcher>) {
        if let Some(ignore) = ignore {
            self.hidden
                .retain(|path, is_dir| !ignore.is_whitelisted(Path::new(path), *is_dir));
        }
    }

    /// `prefix` is the search-root-relative slice of the index root, with a
    /// trailing slash (or empty for the whole index).
    pub fn is_visible(&self, indexed: &str, prefix: &str, include_hidden: bool) -> bool {
        let Some(relative) = indexed.strip_prefix(prefix) else {
            return false;
        };
        if include_hidden {
            return true;
        }
        relative
            .match_indices('/')
            .map(|(offset, _)| prefix.len() + offset)
            .chain(std::iter::once(indexed.len()))
            .all(|end| !self.hidden.contains_key(&indexed[..end]))
    }
}

pub fn is_hidden(path: &Path, metadata: Option<&Metadata>) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.is_some_and(|meta| meta.file_attributes() & 0x2 != 0) {
            return true;
        }
    }
    #[cfg(not(windows))]
    let _ = metadata;
    path.file_name()
        .is_some_and(|name| name.as_encoded_bytes().starts_with(b"."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_hidden_root_does_not_hide_visible_descendants() {
        let mut visibility = PathVisibility::default();
        visibility.record(".github", true, None, None);
        visibility.record(".github/.nested", true, None, None);
        visibility.record(".secret", false, None, None);
        assert!(!visibility.is_visible(".github/settings.txt", "", false));
        assert!(visibility.is_visible(".github/settings.txt", ".github/", false));
        assert!(!visibility.is_visible(".github/.nested/file", ".github/", false));
        assert!(visibility.is_visible(".github/.nested/file", "", true));
        assert!(visibility.is_visible("visible.txt", "", false));
        assert!(!visibility.is_visible(".secret", "", false));
        assert!(!visibility.is_visible("outside/file", ".github/", true));
    }
}

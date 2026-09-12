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
#[serde(from = "HiddenEntries")]
pub struct PathVisibility {
    hidden: BTreeMap<String, bool>,
    /// Attribute-hidden entries cannot be ruled out by inspecting the basename.
    /// Derived on load so the persisted format remains just the barrier map.
    #[serde(skip)]
    non_dot_hidden: usize,
}

#[derive(Deserialize)]
struct HiddenEntries {
    hidden: BTreeMap<String, bool>,
}

impl From<HiddenEntries> for PathVisibility {
    fn from(entries: HiddenEntries) -> Self {
        let non_dot_hidden = entries
            .hidden
            .keys()
            .filter(|path| !has_dot_name(path))
            .count();
        Self {
            hidden: entries.hidden,
            non_dot_hidden,
        }
    }
}

fn has_dot_name(relative: &str) -> bool {
    relative.rsplit('/').next().unwrap().starts_with('.')
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
            let previous = self.hidden.insert(relative.to_string(), is_dir);
            if previous.is_none() && !has_dot_name(relative) {
                self.non_dot_hidden += 1;
            }
            previous != Some(is_dir)
        } else {
            let removed = self.hidden.remove(relative).is_some();
            if removed && !has_dot_name(relative) {
                self.non_dot_hidden -= 1;
            }
            removed
        }
    }

    /// Ignore-file whitelists can admit a hidden entry even without --hidden.
    pub fn apply_ignore_rules(&mut self, ignore: Option<&IgnoreMatcher>) {
        if let Some(ignore) = ignore {
            let mut non_dot_hidden = 0;
            self.hidden.retain(|path, is_dir| {
                let retain = !ignore.is_whitelisted(Path::new(path), *is_dir);
                if retain && !has_dot_name(path) {
                    non_dot_hidden += 1;
                }
                retain
            });
            self.non_dot_hidden = non_dot_hidden;
        }
    }

    /// `prefix` is the search-root-relative slice of the index root, with a
    /// trailing slash (or empty for the whole index).
    pub fn is_visible(&self, indexed: &str, prefix: &str, include_hidden: bool) -> bool {
        let Some(relative) = indexed.strip_prefix(prefix) else {
            return false;
        };
        if include_hidden || self.hidden.is_empty() {
            return true;
        }
        let mut start = prefix.len();
        for component in relative.split('/') {
            let end = start + component.len();
            // Most indexes have only dot-hidden barriers, even on Windows.
            // Ordinary components then need no tree lookup at any path depth.
            if (self.non_dot_hidden != 0 || component.starts_with('.'))
                && self.hidden.contains_key(&indexed[..end])
            {
                return false;
            }
            start = end + 1;
        }
        true
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

    #[test]
    fn imported_attribute_barriers_survive_clones_and_roundtrips() {
        let encoded = r#"{"hidden":{".github":true,".github/attributes":true,"attributes":true,"private.txt":false}}"#;
        let visibility: PathVisibility = serde_json::from_str(encoded).unwrap();
        assert_eq!(visibility.non_dot_hidden, 3);
        for visibility in [
            visibility.clone(),
            serde_json::from_str(&serde_json::to_string(&visibility).unwrap()).unwrap(),
        ] {
            assert!(!visibility.is_visible("attributes/deep/file.txt", "", false));
            assert!(!visibility.is_visible("private.txt", "", false));
            assert!(!visibility.is_visible(".github/attributes/file.txt", ".github/", false));
            assert!(visibility.is_visible("attributes/file.txt", "attributes/", false));
            assert!(visibility.is_visible(".github/attributes/file.txt", "", true));
            assert!(visibility.is_visible("ordinary/deep/file.txt", "", false));
            assert!(!visibility.is_visible("outside/file.txt", "attributes/", false));
        }
        assert_eq!(
            serde_json::to_value(&visibility).unwrap(),
            serde_json::from_str::<serde_json::Value>(encoded).unwrap()
        );
    }

    #[test]
    fn removing_attribute_barriers_updates_the_fast_path() {
        let mut visibility: PathVisibility =
            serde_json::from_str(r#"{"hidden":{"attributes":true,"private.txt":false}}"#).unwrap();
        assert!(visibility.record("attributes", true, None, None));
        assert!(!visibility.record("attributes", true, None, None));
        assert_eq!(visibility.non_dot_hidden, 1);
        assert!(visibility.is_visible("attributes/file.txt", "", false));
        assert!(!visibility.is_visible("private.txt", "", false));
        assert!(visibility.record("private.txt", false, None, None));
        assert_eq!(visibility.non_dot_hidden, 0);
        assert!(visibility.record(".secret", false, None, None));
        assert!(!visibility.record(".secret", false, None, None));
        assert_eq!(visibility.non_dot_hidden, 0);
        assert!(visibility.is_visible("deep/ordinary/file.txt", "", false));
        assert!(!visibility.is_visible(".secret", "", false));
    }

    #[test]
    fn whitelist_removal_updates_attribute_barriers() {
        let temp = tempfile::tempdir().unwrap();
        let mut builder = ignore::gitignore::GitignoreBuilder::new(temp.path());
        builder.add_line(None, "!attributes/").unwrap();
        builder.add_line(None, "!private.txt").unwrap();
        builder.add_line(None, "!.allowed").unwrap();
        let matcher = IgnoreMatcher::new(
            builder.build().unwrap(),
            ignore::gitignore::GitignoreBuilder::new(temp.path())
                .build()
                .unwrap(),
        )
        .unwrap();
        let mut visibility: PathVisibility = serde_json::from_str(
            r#"{"hidden":{"attributes":true,"private.txt":false,".allowed":false,".secret":false}}"#,
        )
        .unwrap();
        visibility.apply_ignore_rules(Some(&matcher));
        assert_eq!(visibility.non_dot_hidden, 0);
        assert!(visibility.is_visible("attributes/deep/file.txt", "", false));
        assert!(visibility.is_visible("private.txt", "", false));
        assert!(visibility.is_visible(".allowed", "", false));
        assert!(!visibility.is_visible(".secret", "", false));
    }

    #[test]
    fn optimized_visibility_matches_barrier_walk() {
        for encoded in [
            r#"{"hidden":{}}"#,
            r#"{"hidden":{".github":true,".github/.nested":true,"src/.secret":false}}"#,
            r#"{"hidden":{".github":true,"src/attributes":true,"src/private.txt":false}}"#,
        ] {
            let visibility: PathVisibility = serde_json::from_str(encoded).unwrap();
            for prefix in ["", ".github/", "src/", "src/attributes/"] {
                for path in [
                    "ordinary/deep/file.txt",
                    ".github/settings.txt",
                    ".github/.nested/file.txt",
                    "src/.secret",
                    "src/.unlisted",
                    "src/attributes/file.txt",
                    "src/attributes-other/file.txt",
                    "src/private.txt",
                ] {
                    for hidden in [false, true] {
                        let expected = path.strip_prefix(prefix).is_some_and(|relative| {
                            hidden
                                || relative
                                    .match_indices('/')
                                    .map(|(offset, _)| prefix.len() + offset)
                                    .chain(std::iter::once(path.len()))
                                    .all(|end| !visibility.hidden.contains_key(&path[..end]))
                        });
                        assert_eq!(
                            visibility.is_visible(path, prefix, hidden),
                            expected,
                            "{encoded}: {path}, prefix={prefix}, hidden={hidden}"
                        );
                    }
                }
            }
        }
    }
}

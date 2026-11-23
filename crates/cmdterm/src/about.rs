//! What the settings' Changelog and Licenses pages show: the changelog and
//! the app's licenses built into the binary, and the third-party crates it
//! links with their licenses, which `build.rs` lists.

/// The changelog, as `CHANGELOG.md` holds it.
pub const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");
/// The app's MIT license.
pub const LICENSE_MIT: &str = include_str!("../../../LICENSE-MIT");
/// The app's Apache 2.0 license.
pub const LICENSE_APACHE: &str = include_str!("../../../LICENSE-APACHE");

/// A crate linked into the app that is not part of it.
#[derive(Debug)]
pub struct ThirdParty {
    pub name: &'static str,
    pub version: &'static str,
    /// The SPDX expression from its manifest, empty when it names a license
    /// file instead.
    pub license: &'static str,
    pub repository: &'static str,
    /// Its license files, as indexes into [`LICENSE_TEXTS`].
    pub texts: &'static [usize],
}

impl ThirdParty {
    /// The texts of its license files.
    pub fn license_texts(&self) -> impl Iterator<Item = &'static str> + use<> {
        self.texts.iter().map(|&index| LICENSE_TEXTS[index])
    }
}

include!(concat!(env!("OUT_DIR"), "/licenses.rs"));

/// One block of the changelog, as the Changelog page lays it out.
#[derive(Debug, PartialEq, Eq)]
pub enum Block {
    /// A release: its version, or `Unreleased`, and its date if it has one.
    Release { version: String, date: String },
    /// A group of changes in a release, such as `Added` or `Fixed`.
    Group(String),
    /// One change, its lines joined, with how deep its bullet is nested.
    Item { depth: usize, text: String },
    /// A paragraph outside a list.
    Paragraph(String),
}

/// Splits a Keep a Changelog file into blocks. The text before the first
/// release, the link definitions at the end and backticks are left out.
pub fn changelog(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut in_release = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(heading) = line.strip_prefix("## ") {
            in_release = true;
            let (version, date) = match heading.split_once(" - ") {
                Some((version, date)) => (version, date.trim()),
                None => (heading, ""),
            };
            let version = version.trim().trim_start_matches('[').trim_end_matches(']');
            blocks.push(Block::Release {
                version: version.to_owned(),
                date: date.to_owned(),
            });
            continue;
        }
        if !in_release || trimmed.is_empty() {
            if trimmed.is_empty() {
                // A blank line ends a paragraph, so the next line starts one.
                blocks.push(Block::Paragraph(String::new()));
            }
            continue;
        }
        if let Some(group) = line.strip_prefix("### ") {
            blocks.push(Block::Group(group.trim().to_owned()));
            continue;
        }
        // A link definition, such as `[0.1.0]: https://...`.
        if trimmed.starts_with('[') && trimmed.contains("]: ") {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            blocks.push(Block::Item {
                depth: indent / 2,
                text: item.to_owned(),
            });
            continue;
        }
        // A line that goes on with the item or paragraph above it.
        match blocks.last_mut() {
            Some(Block::Item { text, .. }) if indent > 0 => {
                text.push(' ');
                text.push_str(trimmed);
            }
            Some(Block::Paragraph(text)) => {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(trimmed);
            }
            _ => blocks.push(Block::Paragraph(trimmed.to_owned())),
        }
    }
    blocks.retain(|block| !matches!(block, Block::Paragraph(text) if text.is_empty()));
    // The page shows plain text, so code spans lose their backticks.
    for block in &mut blocks {
        if let Block::Item { text, .. } | Block::Paragraph(text) = block {
            text.retain(|c| c != '`');
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_changelog_splits_into_releases_groups_and_items() {
        let text = "# Changelog\n\nIntro text.\n\n## [Unreleased]\n\n### Added\n\n\
                    - One `change`\n  that goes on.\n  - A nested one\n- Another\n\n\
                    ## [0.1.0] - 2026-11-22\n\nFirst release.\n\n[0.1.0]: https://example.com\n";
        assert_eq!(
            changelog(text),
            vec![
                Block::Release {
                    version: "Unreleased".into(),
                    date: String::new()
                },
                Block::Group("Added".into()),
                Block::Item {
                    depth: 0,
                    text: "One change that goes on.".into()
                },
                Block::Item {
                    depth: 1,
                    text: "A nested one".into()
                },
                Block::Item {
                    depth: 0,
                    text: "Another".into()
                },
                Block::Release {
                    version: "0.1.0".into(),
                    date: "2026-11-22".into()
                },
                Block::Paragraph("First release.".into()),
            ]
        );
    }

    #[test]
    fn the_shipped_changelog_has_releases() {
        let blocks = changelog(CHANGELOG);
        assert!(matches!(blocks.first(), Some(Block::Release { .. })));
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block, Block::Item { .. }))
        );
    }

    #[test]
    fn the_crate_list_points_at_texts_it_has() {
        for krate in CRATES {
            assert!(!krate.name.is_empty());
            assert!(krate.texts.iter().all(|&index| index < LICENSE_TEXTS.len()));
        }
        assert!(LICENSE_MIT.contains("MIT"));
        assert!(LICENSE_APACHE.contains("Apache"));
    }
}

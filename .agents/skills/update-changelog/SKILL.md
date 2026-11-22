---
name: update-changelog
description: Update CHANGELOG.md after a landed change in this repo. Load this skill when an implementation or bugfix is done and changed user-facing behavior, to write its entry under [Unreleased]; load it at release time to move [Unreleased] under a new version heading and bump the workspace version.
---

# Update the changelog

`CHANGELOG.md` at the repo root is the single place release notes are written. Every
landed implementation or bugfix that changes user-facing behavior gets its bullet
there before the work counts as done. The file follows Keep a Changelog, and a
released section is history: it is never edited.

This is the canonical copy of the skill. `.claude/skills/update-changelog` is a
symlink to this directory so Claude Code discovers it too; edit only this file.

## Add an entry

1. Read `CHANGELOG.md` and the diff of the work (`git diff` against the commit the
   work started from). Completion: you can name each user-facing change the diff
   makes.
2. Write one bullet per change under the matching `###` heading in
   `## [Unreleased]`, adding the heading in the fixed category order below when it is
   missing. Completion: every user-facing change from step 1 has exactly one bullet,
   and no bullet describes a change the diff does not make.
3. Match the style of the released entries. Completion: `git diff CHANGELOG.md`
   shows only new bullets under `[Unreleased]`; no released section moved.

## At release time

When the user asks for a release: move `## [Unreleased]` under a new
`## [x.y.z] - YYYY-MM-DD` heading with today's date, start a fresh empty
`## [Unreleased]`, bump `version` in the root `Cargo.toml` and the `cmdterm*`
entries in `Cargo.lock`, and add the compare and release link refs at the bottom of
the file. Completion: `cargo pkgid -p cmdterm` reports the new version, and the tag
the user pushes matches it, because CI compares `v` + the workspace version with the
tag.

## Categories

Keep a Changelog order: Added, Changed, Deprecated, Removed, Fixed, Security.

| Heading | Use for |
| --- | --- |
| `### Added` | A new user-facing capability: feature, command, setting, extension API, panel. |
| `### Changed` | Existing behavior that now works differently, including defaults and output. |
| `### Deprecated` | Still works but will go away; say what replaces it. |
| `### Removed` | A user-facing capability that no longer exists. |
| `### Fixed` | A bug: something broken now works. |
| `### Security` | A vulnerability fixed or an exposure closed. |

## Style

- One bullet per change, starting with a verb, user-facing effect first, the crate or
  module in backticks when that helps: "Terminals in bubblewrap sandboxes no longer
  exit at once when the app runs from an AppImage."
- Plain, specific language, sentence case, no em dashes, no filler, like every doc in
  this repo.
- Internal refactors, tests and CI churn get no bullet. The changelog says what a
  user of the app or a crate can do differently, not how the code is arranged.
- A fix for something shipped in an earlier release still goes under `[Unreleased]`.

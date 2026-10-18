//! The rail's search: ranks worktrees by how well a query describes them.
//!
//! A worktree is a small document of weighted fields: the title of its last
//! agent session and the ones before, its branch, its project, its folder,
//! the change request of its branch and what its agents wait for. The query
//! is split into words like the fields are, without filler words such as
//! "the" or "wo". A query word matches a field word when they are equal,
//! share a stem ("fixing", "fixed"), one starts the other (while typing),
//! they mean the same in a coding project ("bug" and "fix", "login" and
//! "auth", "Fehler" and "error"), they are one or two typos apart, one
//! contains the other, or the word is the field's initials ("sbs" for
//! `sidebar-search`). A worktree needs at least half of the query words to
//! match; more matched words, better kinds of match and heavier fields rank
//! it higher, and the whole query appearing as typed adds a bonus.
//!
//! It runs on a keystroke in the rail's search field, over a few dozen
//! worktrees, so it may allocate.

use std::path::{Path, PathBuf};

use crate::state::{Project, WorktreeTitles};
use crate::vcs::Worktree;
use crate::workspace::Inner;

/// Results the rail lists at most.
pub const MAX_HITS: usize = 50;

/// Words that carry no meaning in a query, in English and German.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "the", "of", "to", "in", "on", "for", "with", "where", "what", "which",
    "who", "is", "are", "was", "did", "do", "does", "i", "my", "me", "it", "that", "this", "at",
    "by", "from", "or", "about", "der", "die", "das", "den", "dem", "ein", "eine", "einen", "und",
    "oder", "wo", "was", "wie", "ich", "mein", "meine", "ist", "im", "am", "zum", "zur", "mit",
    "von", "fur", "auf", "habe", "hat",
];

/// Words that mean the same thing in a coding project, folded and stemmed
/// like the words they are compared with.
const SYNONYMS: &[&[&str]] = &[
    &[
        "fix", "bug", "bugfix", "hotfix", "issu", "error", "crash", "broken", "defect", "fail",
        "fehl", "regress", "patch",
    ],
    &[
        "feat",
        "featur",
        "add",
        "new",
        "implement",
        "introduc",
        "support",
        "neu",
        "hinzufug",
    ],
    &[
        "auth",
        "login",
        "logout",
        "signin",
        "sign",
        "oauth",
        "credential",
        "password",
        "token",
        "anmeld",
        "sso",
    ],
    &[
        "ui",
        "ux",
        "frontend",
        "view",
        "layout",
        "style",
        "css",
        "design",
        "theme",
        "oberflach",
    ],
    &["test", "spec", "check", "ci", "coverag", "assert"],
    &[
        "doc",
        "docs",
        "readm",
        "document",
        "guid",
        "manual",
        "dokumentation",
    ],
    &[
        "refactor",
        "cleanup",
        "clean",
        "tidy",
        "restructur",
        "simplify",
        "rewrit",
        "aufraum",
    ],
    &[
        "perf", "perform", "speed", "fast", "slow", "optimiz", "optimis", "latency", "schnell",
        "langsam",
    ],
    &["search", "find", "query", "lookup", "filter", "such"],
    &["remov", "delet", "drop", "rm", "entfern", "losch"],
    &[
        "updat", "upgrad", "bump", "dep", "deps", "depend", "aktualis",
    ],
    &[
        "config", "configur", "setting", "set", "prefer", "option", "einstell",
    ],
    &[
        "db",
        "databas",
        "sql",
        "migrat",
        "schema",
        "postgr",
        "sqlit",
        "datenbank",
    ],
    &[
        "api", "endpoint", "rest", "http", "server", "rout", "handler",
    ],
    &["deploy", "releas", "ship", "publish", "rollout"],
    &["title", "nam", "label", "titl", "heading"],
    &[
        "agent",
        "claud",
        "codex",
        "ai",
        "llm",
        "assistant",
        "copilot",
        "gemini",
    ],
    &["term", "terminal", "shell", "pty", "consol", "tty"],
    &["sidebar", "rail", "panel", "seitenleist"],
    &[
        "vcs",
        "git",
        "branch",
        "worktre",
        "repo",
        "repositor",
        "merg",
        "rebas",
        "commit",
    ],
    &["window", "dialog", "modal", "overlay", "popup"],
    &["key", "keybind", "shortcut", "hotkey", "keymap", "tast"],
    &[
        "sandbox",
        "contain",
        "devcontain",
        "docker",
        "podman",
        "isol",
    ],
    &["remot", "ssh", "host", "machin", "vm"],
    &["log", "trac", "debug", "diagnos", "monitor"],
    &[
        "wait",
        "permission",
        "approv",
        "input",
        "question",
        "blocked",
    ],
    &["done", "finish", "complet", "idl"],
    &["busy", "work", "run", "running"],
];

/// A parsed query.
pub struct Query {
    words: Vec<QueryWord>,
    /// The whole query, folded, for the bonus of an exact phrase.
    phrase: String,
}

struct QueryWord {
    text: String,
    stem: String,
    /// Indices into [`SYNONYMS`] of the groups it belongs to.
    groups: Vec<usize>,
}

/// One field's words, ready to match.
struct FieldWords {
    words: Vec<(String, String)>,
    /// The first letter of every word.
    initials: String,
    /// The field folded, for the phrase bonus.
    folded: String,
}

impl FieldWords {
    fn new(text: &str) -> Self {
        let words: Vec<(String, String)> = words(text)
            .into_iter()
            .map(|word| {
                let stem = stem(&word).to_owned();
                (word, stem)
            })
            .collect();
        let initials = words.iter().filter_map(|(w, _)| w.chars().next()).collect();
        Self {
            words,
            initials,
            folded: fold(text),
        }
    }
}

impl Query {
    /// `None` for a query without words.
    pub fn parse(query: &str) -> Option<Self> {
        let all = words(query);
        let meaningful: Vec<String> = all
            .iter()
            .filter(|word| !STOPWORDS.contains(&word.as_str()))
            .cloned()
            .collect();
        // A query of only filler words searches for them.
        let chosen = if meaningful.is_empty() {
            all
        } else {
            meaningful
        };
        if chosen.is_empty() {
            return None;
        }
        let words = chosen
            .into_iter()
            .map(|text| {
                let stem = stem(&text).to_owned();
                let groups = groups_of(&text, &stem);
                QueryWord { text, stem, groups }
            })
            .collect();
        Some(Self {
            words,
            phrase: fold(query.trim()),
        })
    }

    /// How well `fields`, as text and weight, match. `None` when fewer than
    /// half of the query's words match any field.
    pub fn score(&self, fields: &[(&str, f32)]) -> Option<f32> {
        let fields: Vec<(FieldWords, f32)> = fields
            .iter()
            .filter(|(text, _)| !text.is_empty())
            .map(|(text, weight)| (FieldWords::new(text), *weight))
            .collect();
        let mut total = 0.0;
        let mut matched = 0;
        for word in &self.words {
            let best = fields
                .iter()
                .map(|(field, weight)| field_match(word, field) * weight)
                .fold(0.0, f32::max);
            if best > 0.0 {
                matched += 1;
                total += best;
            }
        }
        if matched * 2 < self.words.len() || matched == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let coverage = matched as f32 / self.words.len() as f32;
        let mut score = total * coverage * coverage;
        if self.phrase.chars().count() >= 3 {
            let bonus = fields
                .iter()
                .filter(|(field, _)| field.folded.contains(&self.phrase))
                .map(|(_, weight)| *weight)
                .fold(0.0, f32::max);
            score += bonus;
        }
        Some(score)
    }
}

/// The best match of `word` in `field`, from 0 for none to 1 for equal.
fn field_match(word: &QueryWord, field: &FieldWords) -> f32 {
    let mut best = field
        .words
        .iter()
        .map(|(text, stem)| word_match(word, text, stem))
        .fold(0.0, f32::max);
    let len = word.text.chars().count();
    if best < 0.5
        && len >= 2
        && field.initials.chars().count() >= len
        && field.initials.starts_with(&word.text)
    {
        best = 0.5;
    }
    best
}

/// How well the query `word` matches the field word `text` with `stem`.
fn word_match(word: &QueryWord, text: &str, stem: &str) -> f32 {
    if word.text == text {
        return 1.0;
    }
    if word.stem == stem {
        return 0.9;
    }
    let len = word.text.chars().count();
    let text_len = text.chars().count();
    if text.starts_with(&word.text) {
        // A short prefix says little.
        #[allow(clippy::cast_precision_loss)]
        let ratio = len as f32 / text_len as f32;
        return if len >= 2 { 0.6 + 0.2 * ratio } else { 0.3 };
    }
    if !word.groups.is_empty()
        && groups_of(text, stem)
            .iter()
            .any(|g| word.groups.contains(g))
    {
        return 0.65;
    }
    let allowed = match len {
        0..=3 => 0,
        4..=7 => 1,
        _ => 2,
    };
    if allowed > 0 && text_len.abs_diff(len) <= allowed {
        let distance = edit_distance(&word.text, text, allowed);
        if distance <= allowed {
            #[allow(clippy::cast_precision_loss)]
            return 0.55 - 0.1 * (distance - 1) as f32;
        }
    }
    if len >= 3 && text.contains(&word.text) {
        return 0.45;
    }
    0.0
}

/// The [`SYNONYMS`] groups `text` or its `stem` belongs to.
fn groups_of(text: &str, stem: &str) -> Vec<usize> {
    SYNONYMS
        .iter()
        .enumerate()
        .filter(|(_, group)| group.iter().any(|w| *w == stem || *w == text))
        .map(|(index, _)| index)
        .collect()
}

/// Lowercase, with German umlauts and ß spelled in plain letters.
fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars().flat_map(char::to_lowercase) {
        match c {
            'ä' => out.push('a'),
            'ö' => out.push('o'),
            'ü' => out.push('u'),
            'ß' => out.push_str("ss"),
            c => out.push(c),
        }
    }
    out
}

/// The words of `text`, folded: split at anything not a letter or digit,
/// at a lowercase letter followed by an uppercase one (`camelCase`) and
/// between letters and digits.
fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut previous: Option<char> = None;
    for c in text.chars() {
        if !c.is_alphanumeric() {
            if !word.is_empty() {
                out.push(fold(&word));
                word.clear();
            }
            previous = None;
            continue;
        }
        if let Some(p) = previous {
            let camel = p.is_lowercase() && c.is_uppercase();
            let digits = p.is_numeric() != c.is_numeric();
            if (camel || digits) && !word.is_empty() {
                out.push(fold(&word));
                word.clear();
            }
        }
        word.push(c);
        previous = Some(c);
    }
    if !word.is_empty() {
        out.push(fold(&word));
    }
    out
}

/// `word` without one common English or German ending, keeping at least
/// three letters.
fn stem(word: &str) -> &str {
    const ENDINGS: &[&str] = &[
        "ations", "ation", "ungen", "ments", "ment", "ness", "heit", "keit", "ings", "ing", "ung",
        "ies", "ied", "ers", "er", "ed", "es", "en", "ly", "e", "s",
    ];
    for ending in ENDINGS {
        if let Some(stem) = word.strip_suffix(ending)
            && stem.chars().count() >= 3
        {
            return stem;
        }
    }
    word
}

/// Edits (insert, delete, substitute, swap two neighbors) between `a` and
/// `b`, or anything above `limit` once it is clear the result will be.
fn edit_distance(a: &str, b: &str, limit: usize) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut before: Vec<usize> = Vec::new();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        current[0] = i;
        let mut row_min = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut value = (previous[j] + 1)
                .min(current[j - 1] + 1)
                .min(previous[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                value = value.min(before[j - 2] + 1);
            }
            current[j] = value;
            row_min = row_min.min(value);
        }
        if row_min > limit {
            return limit + 1;
        }
        before = std::mem::replace(&mut previous, current.clone());
    }
    previous[b.len()]
}

/// One worktree the rail's search found.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub project: u64,
    pub path: PathBuf,
    /// Its position in its project's worktree list, which picks its lane.
    pub lane: usize,
    pub score: f32,
}

/// The worktrees of every project in the rail that match `query`, best
/// first, at most [`MAX_HITS`].
pub fn rail_hits(inner: &Inner, query: &str) -> Vec<Hit> {
    let Some(query) = Query::parse(query) else {
        return Vec::new();
    };
    let tr = inner.tr;
    let mut hits = Vec::new();
    for project in &inner.state.projects {
        let snapshot = inner.snapshots.get(&project.id);
        let fallback = [Worktree {
            path: project.root.clone(),
            branch: None,
            head: None,
            is_main: true,
            locked: false,
            prunable: false,
            bare: false,
        }];
        let worktrees: &[Worktree] = match snapshot {
            Some(snapshot) if !project.is_global() => &snapshot.worktrees,
            _ => &fallback,
        };
        for (lane, worktree) in worktrees.iter().enumerate() {
            if worktree.bare {
                continue;
            }
            let branch = if project.is_global() {
                String::new()
            } else {
                crate::ui::branch_name(worktree)
            };
            let config = project.worktree(&worktree.path);
            let titles = config.map_or(&[][..], |config| config.agent_titles.as_slice());
            let sessions = config.map_or(&[][..], |config| config.agent_sessions.as_slice());
            let change = snapshot.and_then(|snapshot| {
                let branch = worktree.branch.as_deref()?;
                snapshot
                    .change_requests
                    .iter()
                    .find(|request| request.source_branch == branch)
            });
            let change = change.map(|c| format!("!{} {}", c.number, c.title));
            let (attention, message) = inner.worktree_attention(project.id, &worktree.path);
            let attention_label = if attention == crate::term::Attention::None {
                ""
            } else {
                tr.attention_label(attention)
            };
            let folder = folder_name(&worktree.path);
            let path = worktree.path.to_string_lossy();
            let mut fields: Vec<(&str, f32)> = vec![
                (branch.as_str(), 3.0),
                (project.name.as_str(), 1.5),
                (folder.as_str(), 1.5),
                (&*path, 0.5),
                (change.as_deref().unwrap_or(""), 2.0),
                (message.as_str(), 1.0),
                (attention_label, 1.0),
            ];
            // The last session is the worktree's title; the ones before it
            // still say what happened there.
            if let Some((last, older)) = titles.split_last() {
                fields.push((last, 3.0));
                fields.extend(older.iter().map(|title| (title.as_str(), 2.0)));
            }
            // So do the titles of the sessions kept for the Agents tab,
            // which go back further.
            fields.extend(
                sessions
                    .iter()
                    .filter_map(|record| record.title.as_deref())
                    .filter(|title| !titles.iter().any(|known| known == title))
                    .map(|title| (title, 2.0)),
            );
            if let Some(score) = query.score(&fields) {
                hits.push(Hit {
                    project: project.id,
                    path: worktree.path.clone(),
                    lane,
                    score,
                });
            }
        }
    }
    // Stable, so equal scores keep the rail's order.
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    hits.truncate(MAX_HITS);
    hits
}

/// The last component of `path`, or all of it.
pub fn folder_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// What the rail calls the worktree at `path` of `project` with `branch`:
/// the last agent session's title when the settings say so and one ran,
/// else the branch. `Some` only for an agent title.
pub fn agent_title<'a>(
    titles: WorktreeTitles,
    project: &'a Project,
    path: &Path,
) -> Option<&'a str> {
    match titles {
        WorktreeTitles::Branch => None,
        WorktreeTitles::Agent => project.worktree(path)?.agent_title(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(query: &str, fields: &[(&str, f32)]) -> Option<f32> {
        Query::parse(query)?.score(fields)
    }

    /// The fields of a worktree with an agent title and a branch.
    fn doc<'a>(title: &'a str, branch: &'a str) -> [(&'a str, f32); 2] {
        [(title, 3.0), (branch, 3.0)]
    }

    fn best<'a>(query: &str, docs: &[(&'a str, [(&'a str, f32); 2])]) -> Option<&'a str> {
        docs.iter()
            .filter_map(|(name, fields)| Some((*name, score(query, fields)?)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(name, _)| name)
    }

    #[test]
    fn words_split_at_case_digits_and_punctuation() {
        assert_eq!(
            words("feature/sidebarSearch-v2 Größe"),
            ["feature", "sidebar", "search", "v", "2", "grosse"]
        );
    }

    #[test]
    fn stems_join_forms_of_one_word() {
        assert_eq!(stem("fixing"), stem("fixes"));
        assert_eq!(stem("filters"), stem("filter"));
        assert_eq!(stem("user"), "user");
    }

    #[test]
    fn typos_and_swaps_count_as_near() {
        assert_eq!(edit_distance("search", "serach", 2), 1);
        assert_eq!(edit_distance("login", "lgoin", 1), 1);
        assert!(edit_distance("search", "branch", 2) > 2);
    }

    #[test]
    fn a_query_finds_worktrees_by_meaning() {
        let docs = [
            (
                "login",
                doc("Fix the broken sign-in form", "fix/oauth-redirect"),
            ),
            (
                "search",
                doc("Add fuzzy search to the rail", "feature/rail-search"),
            ),
            (
                "perf",
                doc("Make PTY sync allocation-free", "perf/grid-sync"),
            ),
        ];
        // Synonyms: no word of the query is in the login worktree's fields.
        assert_eq!(best("auth bug", &docs), Some("login"));
        // Typing: a prefix finds it.
        assert_eq!(best("sea", &docs), Some("search"));
        // A typo.
        assert_eq!(best("serach", &docs), Some("search"));
        // Filler words and a German synonym.
        assert_eq!(best("wo ist die Suche", &docs), Some("search"));
        assert_eq!(best("speed up the terminal", &docs), Some("perf"));
        // Initials of the branch.
        assert_eq!(best("frs", &docs), Some("search"));
        assert_eq!(best("kubernetes", &docs), None);
    }

    #[test]
    fn the_whole_phrase_and_heavier_fields_rank_higher() {
        let exact = score("rail search", &[("Add rail search", 3.0)]).unwrap();
        let scattered = score("rail search", &[("search the rail", 3.0)]).unwrap();
        assert!(exact > scattered);
        let title = score("grid", &[("grid", 3.0)]).unwrap();
        let path = score("grid", &[("grid", 0.5)]).unwrap();
        assert!(title > path);
    }

    #[test]
    fn half_the_words_must_match() {
        assert!(score("rail kubernetes", &[("rail", 1.0)]).is_some());
        assert!(score("rail kubernetes helm", &[("rail", 1.0)]).is_none());
        assert!(Query::parse("  ").is_none());
    }
}

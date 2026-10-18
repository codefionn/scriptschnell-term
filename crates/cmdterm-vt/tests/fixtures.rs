//! Replays the recorded program sessions in `tests/fixtures/` (see `tests/fixture/mod.rs`
//! for the layout).
//!
//! Every fixture is fed to cmdterm-vt in its recorded chunks and again with random
//! splits; all passes must reach the snapshot in `snapshot.txt`. `CMDTERM_VT_BLESS=1`
//! rewrites the snapshots instead of comparing. Differences to wezterm-term that were
//! accepted when the fixtures were recorded are in `COMPAT.md`.

mod fixture;

use fixture::{Fixture, SPLIT_SEED, Split};
use std::path::{Path, PathBuf};

/// Total size of everything under `tests/fixtures/`.
const BUDGET: u64 = 3 << 20;

fn bless() -> bool {
    std::env::var("CMDTERM_VT_BLESS").is_ok_and(|v| !v.is_empty() && v != "0")
}

fn fixture_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(fixture::fixtures_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.join("meta.toml").is_file())
        .collect();
    dirs.sort();
    dirs
}

fn all() -> Vec<Fixture> {
    fixture_dirs()
        .iter()
        .map(|d| fixture::load(d).unwrap_or_else(|e| panic!("{}: {e}", d.display())))
        .collect()
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            files_under(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// The first line where two texts differ, for a readable failure.
fn first_difference(want: &str, got: &str) -> String {
    for (i, (a, b)) in want.lines().zip(got.lines()).enumerate() {
        if a != b {
            return format!("line {}:\n  file: {a}\n  got:  {b}", i + 1);
        }
    }
    format!(
        "the texts have {} and {} lines",
        want.lines().count(),
        got.lines().count()
    )
}

#[test]
fn there_are_fixtures() {
    assert!(
        fixture_dirs().len() >= 2,
        "tests/fixtures/ holds the recorded sessions"
    );
}

fn meta_of(dir: &Path) -> toml::Table {
    let text = std::fs::read_to_string(dir.join("meta.toml")).unwrap();
    text.parse()
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
}

fn meta_str(meta: &toml::Table, key: &str) -> String {
    meta.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// The corpus is made of real programs: at least eight different ones, each with the
/// version it was recorded with.
#[test]
fn corpus_has_at_least_eight_real_programs_with_versions() {
    let mut programs = std::collections::BTreeSet::new();
    for dir in fixture_dirs() {
        let meta = meta_of(&dir);
        let version = meta_str(&meta, "program_version");
        assert!(
            !version.trim().is_empty(),
            "{}: meta.toml has no program_version (record it again)",
            dir.display()
        );
        programs.insert(meta_str(&meta, "program"));
    }
    assert!(
        programs.len() >= 8,
        "only {} different programs: {programs:?}",
        programs.len()
    );
    assert!(fixture_dirs().len() >= 8);
}

/// README.md lists each fixture with its program version, and every scenario is either
/// recorded or listed as not recorded.
#[test]
fn readme_lists_fixtures_and_unrecorded_scenarios() {
    let readme = std::fs::read_to_string(fixture::fixtures_dir().join("README.md"))
        .expect("tests/fixtures/README.md");
    for dir in fixture_dirs() {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let version = meta_str(&meta_of(&dir), "program_version");
        let row = readme
            .lines()
            .find(|l| l.starts_with(&format!("| `{name}` |")))
            .unwrap_or_else(|| panic!("README.md has no table row for fixture {name}"));
        assert!(
            row.contains(&version),
            "README.md row for {name} does not name the version {version:?}"
        );
    }
    let (_, not_recorded) = readme
        .split_once("Not recorded:")
        .expect("README.md needs a `Not recorded:` table");
    let scenarios = fixture::fixtures_dir().join("scenarios");
    let mut seen = 0;
    for e in std::fs::read_dir(scenarios).unwrap().flatten() {
        let path = e.path();
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        seen += 1;
        let recorded = fixture::fixtures_dir()
            .join(&name)
            .join("meta.toml")
            .is_file();
        let listed = not_recorded.contains(&format!("| `{name}` |"));
        assert!(
            recorded != listed,
            "scenario {name}: recorded = {recorded}, listed as not recorded = {listed}; \
             it must be exactly one of the two"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            listed,
            text.contains("NOT RECORDED"),
            "scenario {name}: the NOT RECORDED comment and the README list disagree"
        );
    }
    assert!(seen >= 18, "only {seen} scenarios");
}

#[test]
fn fixtures_replay_to_their_snapshots() {
    for fx in all() {
        let got = fixture::render(&fixture::replay(&fx, Split::Recorded));
        let path = fx.dir.join("snapshot.txt");
        if bless() {
            std::fs::write(&path, &got).unwrap();
        } else {
            let want = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                panic!("{}: {e} (CMDTERM_VT_BLESS=1 writes it)", path.display())
            });
            assert!(
                want == got,
                "{}: snapshot differs, {}\nCMDTERM_VT_BLESS=1 rewrites it.\n{got}",
                fx.name,
                first_difference(&want, &got)
            );
        }
        for i in 0..4 {
            let split = fixture::replay(&fx, Split::Random(SPLIT_SEED + i));
            let split = fixture::render(&split);
            assert!(
                split == got,
                "{}: random split {i} reaches another state, {}",
                fx.name,
                first_difference(&got, &split)
            );
        }
    }
}

#[test]
fn fixtures_stay_within_the_size_budget() {
    let mut files = Vec::new();
    files_under(&fixture::fixtures_dir(), &mut files);
    let total: u64 = files
        .iter()
        .map(|f| std::fs::metadata(f).unwrap().len())
        .sum();
    assert!(
        total < BUDGET,
        "fixtures take {total} bytes, budget {BUDGET}"
    );
}

#[test]
fn fixtures_hold_no_home_paths() {
    let mut files = Vec::new();
    files_under(&fixture::fixtures_dir(), &mut files);
    for f in files {
        let bytes = std::fs::read(&f).unwrap();
        let leaks = fixture::path_leaks(&bytes);
        assert!(leaks.is_empty(), "{} contains {leaks:?}", f.display());
    }
}

#[test]
fn chunk_lists_round_trip() {
    let text = "chunk 5\nresize 10 20\nchunk 7\n";
    let events = fixture::parse_chunks(text).unwrap();
    assert_eq!(fixture::format_chunks(&events), text);
    assert!(fixture::parse_chunks("chunk x\n").is_err());
    assert!(fixture::parse_chunks("resize 1\n").is_err());
}

#[test]
fn random_splits_cover_every_byte_and_keep_resizes() {
    use fixture::Ev;
    let events = vec![Ev::Chunk(300), Ev::Resize(5, 6), Ev::Chunk(40)];
    let plan = fixture::plan(&events, Split::Random(SPLIT_SEED));
    let bytes: usize = plan
        .iter()
        .map(|e| if let Ev::Chunk(n) = e { *n } else { 0 })
        .sum();
    assert_eq!(bytes, 340);
    let resizes: Vec<_> = plan
        .iter()
        .filter(|e| matches!(e, Ev::Resize(..)))
        .collect();
    assert_eq!(resizes.len(), 1);
    // The resize still sits between the two recorded chunks.
    let at = plan
        .iter()
        .position(|e| matches!(e, Ev::Resize(..)))
        .unwrap();
    let before: usize = plan[..at]
        .iter()
        .map(|e| if let Ev::Chunk(n) = e { *n } else { 0 })
        .sum();
    assert_eq!(before, 300);
}

# Fixtures

Recorded sessions of real programs, replayed by `tests/fixtures.rs`. CI only replays
them: no PTY and no external program runs in `cargo test`.

## Layout

One directory per fixture, named like its scenario in `scenarios/<name>.toml`:

| File | Content |
| --- | --- |
| `output.bin` | Every byte the program wrote to the terminal. |
| `chunks.txt` | The sizes of the reads, and the resize steps between them. |
| `meta.toml` | Program, arguments, version of the program, size, the cleaned environment. |
| `snapshot.txt` | The golden state of cmdterm-vt after the last byte: text per row with clusters, style runs, cursor, title, modes and scrollback. |

The replay test feeds `output.bin` in its recorded chunks and in four passes with random
chunk splits (fixed seed). All must reach `snapshot.txt`. Differences to wezterm-term that
were accepted when the corpus was recorded are in `crates/cmdterm-vt/COMPAT.md`. Fixtures
never hold a path under a home directory (the test scans for the Linux and macOS prefixes),
and all files together stay under 3 MiB (the corpus uses well under 100 KiB).

## Scenarios

`scenarios/<name>.toml` says how a fixture is recorded. Record or re-record one with

```sh
cargo run -p cmdterm-vt --example record -- crates/cmdterm-vt/tests/fixtures/scenarios/<name>.toml
```

The example runs the program on a PTY from `cmdterm_env::pty` and writes the fixture and its
snapshot. Read the new `snapshot.txt` before you commit it: no second emulator checks it any
more, so note any difference from xterm in `COMPAT.md`. The program starts in a cleared
environment: a fixed `PATH`, `TERM=xterm-256color`, `LANG=C.UTF-8`, `PS1='$ '`, and
`HOME` and the XDG directories in a scratch directory. A fixture must hold no `$USER`,
`$LOGNAME`, `$HOME`, host name or home-directory path.

A scenario file has `command`, `args`, `rows`, `cols`, `[env]`, `[files]` (created in the
working directory) and a script of `[[step]]` tables: `wait_for`, `wait_quiet_ms`, `send`
(escapes like `\u0012` for Ctrl-R work), `resize = [rows, cols]`, `sleep_ms` and `stop`.
Without `stop` the program must end by itself. With `stop = true` the session ends at that
step, the program is killed, and the fixture keeps whatever it showed (an alternate screen,
mouse modes). Other keys:

- `version_command`: the command whose first line is stored as `program_version`, for
  scenarios that run a shell wrapper (default `<command> --version`).
- `host_paths`: variables whose value is a path under the real home, written `~/...`
  (`RUSTUP_HOME`). The path reaches the program and is not stored.
- `cleanup`: a command that stops what the program left behind (a tmux server).
- `timeout_ms`: longest wait for a step.

Output depends on timing, so recording twice gives different bytes. That is why
the bytes are stored: the replay is deterministic.

`cargo-error` prints its working directory. Record it with `TMPDIR=/tmp` so the scratch path
is `/tmp/cmdterm-vt-record-cargo-error/work` and holds no home directory.

## Bless

```sh
CMDTERM_VT_BLESS=1 cargo test -p cmdterm-vt --test fixtures
```

rewrites every `snapshot.txt` from the bytes in the repository. Read the diff of the
snapshots before you commit them.

## Fixtures

Versions are the ones the corpus was recorded with (2026-11-01, Linux).

| Fixture | Program and version | What it covers |
| --- | --- | --- |
| `ls-color` | ls (GNU coreutils) 9.12 | SGR colors from `ls --color` |
| `ls-resize` | ls (GNU coreutils) 9.12 | Reflow on a narrowing resize. Differs from wezterm-term, see COMPAT.md. |
| `sgr-printf` | GNU bash, version 5.3.20(1)-release (x86_64-pc-linux-gnu) (run as `sh`) | Attributes, truecolor, wide and combining text, cursor moves, title |
| `bash-session` | GNU bash, version 5.3.20(1)-release (x86_64-pc-linux-gnu) | Interactive prompt, Ctrl-R, Ctrl-L, a wrapped line |
| `bash-resize` | GNU bash, version 5.3.20(1)-release (x86_64-pc-linux-gnu) | Shell redraw after SIGWINCH, narrower then wider |
| `zsh-session` | zsh 5.9.2 (x86_64-pc-linux-gnu) | Line editor redraws, Ctrl-R, clear screen |
| `vim-edit` | VIM - Vi IMproved 9.2 (2026 Feb 14, compiled Oct 12 2026 22:40:28) | Alternate screen, scrolling, split windows, wide and combining text |
| `vim-from-bash` | VIM - Vi IMproved 9.2 (2026 Feb 14, compiled Oct 12 2026 22:40:28) | The alternate screen restores the shell screen on quit |
| `nvim-mouse` | NVIM v0.12.6 | Mouse reporting (button events, SGR), focus events, bracketed paste |
| `less-search` | less 710 (PCRE2 regular expressions) | Paging, search highlight, application keypad |
| `htop-pid` | htop 3.5.3-1.1-arch | Meters, reverse video, ECH, mouse mode. Differed from wezterm-term by `erased-cells-keep-pen`, see COMPAT.md. |
| `git-log-diff` | git version 2.56.0 | `log --graph --color`, colored diff, fixed ids |
| `cargo-error` | cargo 1.97.1 (c980f4866 2026-06-30) | Colored diagnostics, bold runs |
| `progress-bar` | GNU bash, version 5.3.20(1)-release (x86_64-pc-linux-gnu) (run as `sh`) | CR with EL, backspace, strike and reverse |
| `python-unicode` | Python 3.14.8 | Emoji, ZWJ, flag, CJK, OSC 8, OSC 133, tabs, wide characters at the edge. Differs from wezterm-term, see COMPAT.md. |
| `python-clusters` | Python 3.14.8 | Heart with VS16 and a Devanagari conjunct. Differs from wezterm-term, see COMPAT.md. |
| `reflow-styled` | cat (GNU coreutils) 9.12 | Reflow of colored, wide and combining text across narrow, wide and narrow again. Differs from wezterm-term, see COMPAT.md. |
| `tmux-panes` | tmux 3.8 | Panes with scroll regions, copy mode, status line |

Not recorded:

| Scenario | Why |
| --- | --- |
| `screen-regions` | GNU screen is not installed on the recording machine. |
| `top-one` | top prints the number of logged-in users in its first frame. The word "user" is this machine's login, so the tool refuses the output. Record it on a machine with another login name. |

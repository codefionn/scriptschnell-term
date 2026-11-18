//! Images from this machine's clipboard for programs in terminals that
//! cannot read it themselves: on an SSH host, a remote machine, a VM, in a
//! container or a sandbox.
//!
//! Two ways get the image there:
//!
//! - Ctrl+Shift+V with an image and no text on the clipboard saves the image
//!   as a PNG file where the terminal runs and pastes the file's path.
//!   Coding agents such as Claude Code attach a pasted image path as the
//!   image. On Windows the path uses forward slashes, because Claude Code
//!   reads a backslash in pasted text as an escape.
//! - Ctrl+V and Alt+V, the keys agents read the clipboard on, first copy the
//!   image to `clipboard.png` there and then reach the program. `xclip` and
//!   `wl-paste` shims hand that file to programs that ask for an image
//!   during the next minute and pass every other call to the real tool. On
//!   another machine and in a container the shims go into `~/.local/bin`;
//!   a bubblewrap sandbox gets them bound over this machine's own tools
//!   ([`sandbox_shims`]).
//!
//! The image leaves this machine only when the user presses one of these
//! keys, so a program in a terminal cannot read the clipboard on its own.
//!
//! The other way, text copied in such a terminal reaches this machine's
//! clipboard as OSC 52, which every terminal accepts ([`copy_text`]). The
//! same shims, along with `xsel` and `wl-copy` ones, turn a program's copy
//! into OSC 52 written to its terminal, so agents that copy through these
//! tools copy from a sandbox too.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use cmdterm_env::{Environment, SandboxKind};

/// The file name, without `.png`, of the image Ctrl+V and Alt+V stage.
pub const BRIDGE_NAME: &str = "clipboard";
/// The tools the shims stand in for.
const TOOLS: [&str; 4] = ["xclip", "xsel", "wl-copy", "wl-paste"];

/// The shim script; `@DIR@` becomes the quoted directory images are staged
/// in. `SCRIPTSCHNELL_TERM_CLIPBOARD_TTY` names the terminal in tests.
const SHIM: &str = r#"#!/bin/sh
# scriptschnell-term clipboard shim. A program in a scriptschnell-term
# terminal that cannot reach your clipboard copies through this script: it
# sends the text to the terminal as OSC 52 and the app puts it on the
# clipboard. When you press Ctrl+V or Alt+V, the app puts the clipboard's
# image in clipboard.png below. For a minute this script hands that image
# to programs that ask xclip or wl-paste for one. Every other call goes to
# the real tool, if there is one.
dir=@DIR@
tool=${0##*/}
image=$dir/clipboard.png
tty=${SCRIPTSCHNELL_TERM_CLIPBOARD_TTY:-/dev/tty}
out= in= list= type= sel= trim= clear= words= files= prev=
for arg in "$@"; do
  case $prev in
    -t|--type) type=$arg; prev=; continue ;;
    -selection|-sel|-se) sel=$arg; prev=; continue ;;
    -s|--seat) if [ "$tool" = wl-copy ]; then prev=; continue; fi ;;
  esac
  case $arg in
    -o|-out|--output) out=1 ;;
    -i|-in|--input) in=1 ;;
    -l|--list-types) list=1 ;;
    --type=*) type=${arg#--type=} ;;
    -b|--clipboard) sel=clipboard ;;
    -p|--primary) sel=primary ;;
    -n|--trim-newline) trim=1 ;;
    -c|--clear) clear=1 ;;
    -*) ;;
    *) words="${words:+$words }$arg"; files=1 ;;
  esac
  prev=$arg
done
# Copies: wl-copy always copies, xclip unless it pastes, xsel when told to
# or fed by a pipe. xclip and xsel default to the primary selection, which
# the app leaves alone. Images and files go to the real tool.
copy=
case $tool in
  wl-copy) copy=1; sel=${sel:-clipboard} ;;
  xclip) [ -z "$out" ] && [ -z "$files" ] && copy=1; words= trim= clear= ;;
  xsel) { [ -n "$in" ] || [ -n "$clear" ] || { [ -z "$out" ] && ! [ -t 0 ]; }; } && copy=1; words= trim= ;;
esac
case $type in image/*) copy= ;; esac
# Opening /dev/tty fails without a terminal. `true`, unlike `:`, survives
# a failed redirection.
if [ -n "$copy" ] && { true >"$tty"; } 2>/dev/null; then
  case $sel in c*) sel=c ;; *) sel=p ;; esac
  if [ -n "$clear" ]; then
    data=
  elif [ -n "$words" ]; then
    data=$(printf '%s' "$words" | base64 | tr -d '\n')
  elif [ -n "$trim" ]; then
    data=$(printf '%s' "$(cat)" | base64 | tr -d '\n')
  else
    data=$(base64 | tr -d '\n')
  fi
  printf '\033]52;%s;%s\007' "$sel" "$data" >"$tty"
  exit 0
fi
if [ -z "$copy" ] && [ -n "$(find "$image" -mmin -1 2>/dev/null)" ]; then
  [ "$tool" = wl-paste ] && out=1
  [ "$type" = TARGETS ] && list=1
  if [ -n "$out" ]; then
    [ -n "$list" ] && { echo image/png; exit 0; }
    case $type in
      image/png) exec cat "$image" ;;
      image/*) exit 1 ;;
    esac
  fi
fi
IFS=:
for d in $PATH; do
  [ -x "$d/$tool" ] && ! [ "$d/$tool" -ef "$0" ] && exec "$d/$tool" "$@"
done
echo "$tool: this machine has no clipboard; scriptschnell-term copies text sent to its terminal and hands over images on Ctrl+V" >&2
exit 1
"#;

/// Saves the image read from stdin as `$1.png` in the staging directory and
/// prints its path. With a shim in `$2`, also installs it as each tool in
/// `~/.local/bin`, unless a file there is not one of ours. Pasted images
/// older than a day go away.
const STAGE: &str = r#"umask 077
dir=$HOME/.cache/scriptschnell-term-clipboard
mkdir -p "$dir" || exit 1
find "$dir" -name 'paste-*.png' -mmin +1440 -exec rm -f {} + 2>/dev/null
cat > "$dir/$1.part" && mv -f "$dir/$1.part" "$dir/$1.png" || exit 1
if [ -n "$2" ] && mkdir -p "$HOME/.local/bin"; then
  for tool in xclip xsel wl-copy wl-paste; do
    shim=$HOME/.local/bin/$tool
    if [ ! -e "$shim" ] || grep -q 'scriptschnell-term clipboard shim' "$shim" 2>/dev/null; then
      printf '%s' "$2" > "$shim.part" && chmod 755 "$shim.part" && mv -f "$shim.part" "$shim"
    fi
  done
fi
printf '%s\n' "$dir/$1.png"
"#;

/// The clipboard [`copy_text`] writes to, open for the life of the app: on X11 and
/// Wayland a copy lasts only as long as the connection that made it.
static COPIED: Mutex<schnellui::platform::SystemClipboard> =
    Mutex::new(schnellui::platform::SystemClipboard::new());

/// Puts `text` on the clipboard: what a program sent with OSC 52, or the
/// debug browser's selection. Called off the UI thread, so the window
/// system's round trip runs on a thread of its own.
pub fn copy_text(text: String) {
    let _ = std::thread::Builder::new()
        .name("clipboard-copy".into())
        .spawn(move || {
            let mut clipboard = COPIED.lock().unwrap_or_else(PoisonError::into_inner);
            // A program cannot be told that the copy failed.
            let _ = clipboard.write_text(text);
        });
}

/// Where a terminal's program finds the images pasted into it.
#[derive(Clone, Debug)]
pub enum Target {
    /// In `dir` on this machine: the program runs here, maybe in a sandbox
    /// that can read `dir`. `bridge` when it cannot read the clipboard
    /// itself, as in a bubblewrap sandbox.
    Here { dir: PathBuf, bridge: bool },
    /// On another file system, written through this environment.
    There(Environment),
}

impl Target {
    /// The target for a terminal in `env`. `dir` is this machine's staging
    /// directory, see [`staging_dir`].
    pub fn of(env: &Environment, dir: &Path) -> Self {
        match env {
            Environment::Local => Self::Here {
                dir: dir.to_path_buf(),
                bridge: false,
            },
            Environment::Sandbox(target) if target.host.is_local() => Self::Here {
                dir: dir.to_path_buf(),
                bridge: target.policy.kind == SandboxKind::Bwrap,
            },
            // The sandbox sees its machine's home read-only, shims included.
            Environment::Sandbox(target) => Self::There(target.host.base().clone()),
            other => Self::There(other.clone()),
        }
    }

    /// Whether Ctrl+V and Alt+V should stage the clipboard's image first.
    pub fn bridges(&self) -> bool {
        match self {
            Self::Here { bridge, .. } => *bridge,
            Self::There(_) => true,
        }
    }

    /// Saves `png` as `name.png` and returns its path as the program sees
    /// it. Blocking; run it off the UI thread.
    pub fn stage(&self, png: &[u8], name: &str) -> Result<String, String> {
        match self {
            Self::Here { dir, .. } => {
                prune(dir);
                let path = dir.join(format!("{name}.png"));
                let part = dir.join(format!("{name}.part"));
                std::fs::write(&part, png)
                    .and_then(|()| std::fs::rename(&part, &path))
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                Ok(program_path(&path))
            }
            Self::There(env) => {
                let shim = if name == BRIDGE_NAME {
                    shim(r#""$HOME/.cache/scriptschnell-term-clipboard""#)
                } else {
                    String::new()
                };
                let out = env
                    .command("sh")
                    .args(["-c", STAGE, "sh", name, &shim])
                    .stdout_with_input(png)
                    .map_err(|error| error.to_string())?;
                let path = String::from_utf8_lossy(&out).trim().to_owned();
                if path.is_empty() {
                    return Err("the image was not saved".into());
                }
                Ok(path)
            }
        }
    }
}

/// This machine's staging directory, created private. Sandboxes read it.
pub fn staging_dir(cache: &Path) -> PathBuf {
    let dir = cache.join("clipboard");
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    dir
}

/// `value` as one single-quoted shell word.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The shim with `dir`, a shell word, as its staging directory.
fn shim(dir: &str) -> String {
    SHIM.replace("@DIR@", dir)
}

/// Shims a bubblewrap sandbox binds over this machine's clipboard tools,
/// as `(shim, tool)`: one for each tool found on `PATH`, written to
/// `dir/bin`. A tool that is missing gets none, since bubblewrap mounts a
/// file only over one that exists. An unchanged shim is left alone, so
/// sandboxes that bound it keep reading it.
pub fn sandbox_shims(dir: &Path) -> Vec<(PathBuf, PathBuf)> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    let content = shim(&quote(&dir.to_string_lossy()));
    let bin = dir.join("bin");
    let mut shims = Vec::new();
    for tool in TOOLS {
        let Some(real) = std::env::split_paths(&path)
            .map(|d| d.join(tool))
            .find(|p| p.is_file())
            .and_then(|p| cmdterm_env::canonicalize(p).ok())
        else {
            continue;
        };
        let file = bin.join(tool);
        if std::fs::read(&file).is_ok_and(|old| old == content.as_bytes())
            || write_executable(&file, &content).is_ok()
        {
            shims.push((file, real));
        }
    }
    shims
}

fn write_executable(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// Removes pasted images older than a day.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let day = std::time::Duration::from_secs(24 * 60 * 60);
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|time| time.elapsed().is_ok_and(|age| age > day));
        if old && entry.file_name().to_string_lossy().starts_with("paste-") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `path` as a program on this machine reads it from pasted text: with
/// forward slashes on Windows.
fn program_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        text.strip_prefix(r"\\?\")
            .unwrap_or(&text)
            .replace('\\', "/")
    } else {
        text.into_owned()
    }
}

/// The text to paste for an image saved at `path`: the path, in double
/// quotes when it has whitespace. Agents strip the quotes.
pub fn paste_text(path: &str) -> String {
    if path.chars().any(char::is_whitespace) {
        format!("\"{path}\"")
    } else {
        path.to_owned()
    }
}

/// A unique name for a pasted image.
pub fn paste_name() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    format!("paste-{millis}")
}

/// Encodes clipboard pixels as PNG.
pub fn encode_png(image: &schnellui::platform::ClipboardImage) -> Result<Vec<u8>, String> {
    use image::ImageEncoder;
    let width = u32::try_from(image.width).map_err(|e| e.to_string())?;
    let height = u32::try_from(image.height).map_err(|e| e.to_string())?;
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&image.rgba, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|error| error.to_string())?;
    Ok(png)
}

/// The clipboard's image as PNG, `None` when it holds none. Opens its own
/// clipboard connection, so it runs on any thread.
pub fn read_png() -> Option<Result<Vec<u8>, String>> {
    let image = schnellui::platform::SystemClipboard::new()
        .read_image()
        .ok()?;
    Some(encode_png(&image))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In every shim; [`STAGE`] replaces only files that have it.
    const SHIM_MARKER: &str = "scriptschnell-term clipboard shim";

    #[test]
    fn stage_knows_its_shims() {
        assert!(SHIM.contains(SHIM_MARKER));
        assert!(STAGE.contains(SHIM_MARKER));
        for tool in TOOLS {
            assert!(STAGE.contains(tool));
        }
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    fn run_shim(shim: &Path, args: &[&str], path: &str) -> std::process::Output {
        // A test thread that forks while another writes a script makes
        // running it fail with ETXTBSY for a moment.
        for _ in 0..50 {
            let output = Environment::Local
                .command(shim.as_os_str())
                .args(args)
                .var("PATH", path)
                .output();
            match output {
                Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {}
                Ok(output) if String::from_utf8_lossy(&output.stderr).contains("busy") => {}
                other => return other.unwrap(),
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("{} stayed busy", shim.display());
    }

    #[test]
    #[cfg(unix)]
    fn the_shim_serves_a_fresh_image_and_passes_everything_else_on() {
        let scratch = tempfile::tempdir().unwrap();
        let dir = scratch.path().join("stage");
        let bin = scratch.path().join("bin");
        let real = scratch.path().join("real");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&real).unwrap();
        for tool in TOOLS {
            write_executable(&bin.join(tool), &shim(&quote(&dir.to_string_lossy()))).unwrap();
            write_executable(&real.join(tool), "#!/bin/sh\necho real \"$@\"\n").unwrap();
        }
        let path = format!("{}:{}:/usr/bin:/bin", bin.display(), real.display());
        let xclip = bin.join("xclip");
        let wl_paste = bin.join("wl-paste");

        // Nothing staged: the real tool answers.
        let out = run_shim(&xclip, &["-selection", "clipboard", "-o"], &path);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "real -selection clipboard -o\n"
        );

        std::fs::write(dir.join("clipboard.png"), b"PNG").unwrap();
        let out = run_shim(
            &xclip,
            &["-selection", "clipboard", "-t", "TARGETS", "-o"],
            &path,
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), "image/png\n");
        let out = run_shim(
            &xclip,
            &["-selection", "clipboard", "-t", "image/png", "-o"],
            &path,
        );
        assert_eq!(out.stdout, b"PNG");
        let out = run_shim(
            &xclip,
            &["-selection", "clipboard", "-t", "image/bmp", "-o"],
            &path,
        );
        assert!(!out.status.success());
        let out = run_shim(&wl_paste, &["-l"], &path);
        assert_eq!(String::from_utf8_lossy(&out.stdout), "image/png\n");
        let out = run_shim(&wl_paste, &["--type", "image/png"], &path);
        assert_eq!(out.stdout, b"PNG");
        // Text still comes from the real tool.
        let out = run_shim(&wl_paste, &[], &path);
        assert_eq!(String::from_utf8_lossy(&out.stdout), "real\n");

        // Without a real tool the shim says so.
        let out = run_shim(&wl_paste, &[], &format!("{}:/usr/bin:/bin", bin.display()));
        if !std::path::Path::new("/usr/bin/wl-paste").exists()
            && !std::path::Path::new("/bin/wl-paste").exists()
        {
            assert!(!out.status.success());
            assert!(String::from_utf8_lossy(&out.stderr).contains("no clipboard"));
        }
    }

    /// Runs `shim` with `input` on stdin and a file standing in for its
    /// terminal, and returns what it wrote there.
    #[cfg_attr(not(unix), allow(dead_code))]
    fn copy_through(shim: &Path, args: &[&str], input: &[u8], path: &str) -> String {
        let tty = tempfile::NamedTempFile::new().unwrap();
        for _ in 0..50 {
            let copied = Environment::Local
                .command(shim.as_os_str())
                .args(args)
                .var("PATH", path)
                .var("SCRIPTSCHNELL_TERM_CLIPBOARD_TTY", tty.path())
                .stdout_with_input(input);
            match copied {
                Err(error) if error.to_string().contains("busy") => {}
                other => {
                    other.unwrap();
                    return std::fs::read_to_string(tty.path()).unwrap();
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("{} stayed busy", shim.display());
    }

    #[test]
    #[cfg(unix)]
    fn the_shim_sends_copies_to_the_terminal() {
        let scratch = tempfile::tempdir().unwrap();
        let bin = scratch.path().join("bin");
        let real = scratch.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        for tool in TOOLS {
            write_executable(&bin.join(tool), &shim("/nonexistent")).unwrap();
            write_executable(&real.join(tool), "#!/bin/sh\necho real \"$@\" >&2\n").unwrap();
        }
        let path = format!("{}:{}:/usr/bin:/bin", bin.display(), real.display());
        let tool = |name: &str| bin.join(name);
        // "hi\n" in base64.
        let hi = "\x1b]52;c;aGkK\x07";

        assert_eq!(
            copy_through(&tool("xclip"), &["-selection", "clipboard"], b"hi\n", &path),
            hi
        );
        assert_eq!(
            copy_through(&tool("xclip"), &["-sel", "clip", "-i"], b"hi\n", &path),
            hi
        );
        assert_eq!(
            copy_through(&tool("xsel"), &["--clipboard", "--input"], b"hi\n", &path),
            hi
        );
        assert_eq!(copy_through(&tool("xsel"), &["-b"], b"hi\n", &path), hi);
        assert_eq!(copy_through(&tool("wl-copy"), &[], b"hi\n", &path), hi);
        assert_eq!(
            copy_through(&tool("wl-copy"), &["-n"], b"hi\n", &path),
            "\x1b]52;c;aGk=\x07"
        );
        assert_eq!(
            copy_through(
                &tool("wl-copy"),
                &["--type", "text/plain", "hi", "there"],
                b"",
                &path
            ),
            "\x1b]52;c;aGkgdGhlcmU=\x07"
        );
        assert_eq!(
            copy_through(&tool("wl-copy"), &["--primary"], b"hi\n", &path),
            "\x1b]52;p;aGkK\x07"
        );
        // xclip defaults to the primary selection.
        assert_eq!(
            copy_through(&tool("xclip"), &[], b"hi\n", &path),
            "\x1b]52;p;aGkK\x07"
        );
        assert_eq!(
            copy_through(&tool("wl-copy"), &["--clear"], b"", &path),
            "\x1b]52;c;\x07"
        );

        // Pastes, images and files go to the real tool.
        for args in [
            ["-selection", "clipboard", "-o"].as_slice(),
            ["-selection", "clipboard", "-t", "image/png"].as_slice(),
            ["-selection", "clipboard", "notes.txt"].as_slice(),
        ] {
            assert_eq!(copy_through(&tool("xclip"), args, b"hi\n", &path), "");
        }
        assert_eq!(copy_through(&tool("xsel"), &["-b", "-o"], b"", &path), "");
        assert_eq!(copy_through(&tool("wl-paste"), &[], b"", &path), "");

        // Without a terminal the real tool copies.
        let out = Environment::Local
            .command(tool("wl-copy").as_os_str())
            .var("PATH", &path)
            .var("SCRIPTSCHNELL_TERM_CLIPBOARD_TTY", "/nonexistent/tty")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stderr), "real\n");
    }

    #[test]
    #[cfg(unix)]
    fn staging_through_an_environment_installs_the_shims() {
        let scratch = tempfile::tempdir().unwrap();
        let home = scratch.path();
        // A command run with its own home stands in for another machine.
        let staged = Environment::Local
            .command("sh")
            .args(["-c", STAGE, "sh", BRIDGE_NAME, &shim("\"$HOME/x\"")])
            .var("HOME", home)
            .stdout_with_input(b"PNG")
            .unwrap();
        let staged = String::from_utf8(staged).unwrap();
        let expected = home.join(".cache/scriptschnell-term-clipboard/clipboard.png");
        assert_eq!(staged.trim(), expected.to_string_lossy());
        assert_eq!(std::fs::read(&expected).unwrap(), b"PNG");
        for tool in TOOLS {
            let shim = std::fs::read_to_string(home.join(".local/bin").join(tool)).unwrap();
            assert!(shim.contains(SHIM_MARKER));
        }

        // A user's own tool of that name stays.
        std::fs::write(home.join(".local/bin/xclip"), "mine").unwrap();
        Environment::Local
            .command("sh")
            .args(["-c", STAGE, "sh", BRIDGE_NAME, &shim("x")])
            .var("HOME", home)
            .stdout_with_input(b"PNG")
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(home.join(".local/bin/xclip")).unwrap(),
            "mine"
        );

        // A pasted image installs nothing.
        let other = tempfile::tempdir().unwrap();
        Environment::Local
            .command("sh")
            .args(["-c", STAGE, "sh", "paste-1", ""])
            .var("HOME", other.path())
            .stdout_with_input(b"PNG")
            .unwrap();
        assert!(!other.path().join(".local/bin").exists());
    }

    #[test]
    fn local_images_land_in_the_staging_dir() {
        let scratch = tempfile::tempdir().unwrap();
        let dir = staging_dir(scratch.path());
        let target = Target::of(&Environment::Local, &dir);
        assert!(!target.bridges());
        let path = target.stage(b"PNG", "paste-1").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"PNG");
        assert!(Target::of(&Environment::ssh("box"), &dir).bridges());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_bubblewrap_sandbox_reads_the_staged_image_through_its_shim() {
        let scratch = tempfile::tempdir().unwrap();
        let dir = cmdterm_env::canonicalize(staging_dir(scratch.path())).unwrap();
        let shims = sandbox_shims(&dir);
        if shims.is_empty() {
            eprintln!("skipped: neither xclip nor wl-paste is installed");
            return;
        }
        let mut policy =
            cmdterm_env::SandboxPolicy::local(Vec::new(), std::slice::from_ref(&dir), false);
        policy.readable.push(dir.clone());
        policy.files.extend(shims.iter().cloned());
        let env = Environment::Sandbox(Box::new(cmdterm_env::SandboxTarget::new(
            Environment::Local,
            policy,
        )));
        if env
            .command("true")
            .output()
            .map_or(true, |o| !o.status.success())
        {
            eprintln!("skipped: bubblewrap cannot run here");
            return;
        }
        let target = Target::of(&env, &dir);
        assert!(target.bridges());
        target.stage(b"PNG", BRIDGE_NAME).unwrap();
        for (_, tool) in &shims {
            let name = tool.file_name().unwrap().to_string_lossy();
            if name != "xclip" && name != "wl-paste" {
                continue;
            }
            let out = env
                .command(tool.as_os_str())
                .args(if name == "xclip" {
                    ["-selection", "clipboard", "-t", "image/png", "-o"].as_slice()
                } else {
                    ["--type", "image/png"].as_slice()
                })
                .stdout()
                .unwrap();
            assert_eq!(out, b"PNG", "{name}");
        }
    }

    #[test]
    fn pasted_paths_with_spaces_are_quoted() {
        assert_eq!(paste_text("/tmp/a.png"), "/tmp/a.png");
        assert_eq!(paste_text("C:/Users/A B/a.png"), "\"C:/Users/A B/a.png\"");
    }

    #[test]
    fn clipboard_pixels_encode_as_png() {
        let image = schnellui::platform::ClipboardImage {
            width: 2,
            height: 1,
            rgba: vec![255, 0, 0, 255, 0, 255, 0, 255],
        };
        let png = encode_png(&image).unwrap();
        assert_eq!(&png[..4], b"\x89PNG");
    }
}

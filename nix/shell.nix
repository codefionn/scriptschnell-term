{
  lib,
  mkShell,
  rustToolchain,
  scriptschnell-term,
  libffi,
  bubblewrap,
  git,
  mercurial,
}:

mkShell {
  name = "scriptschnell-term";
  inputsFrom = [ scriptschnell-term ];
  packages = [
    rustToolchain
    # The tests start terminals in bubblewrap sandboxes and drive Git and Mercurial.
    bubblewrap
    git
    mercurial
  ];
  # cargo run builds a binary without the package's RUNPATH.
  LD_LIBRARY_PATH = lib.makeLibraryPath scriptschnell-term.runtimeLibraries;
  # See the package: Oximond's build script looks up libffi.so here.
  LIBRARY_PATH = lib.makeLibraryPath [ libffi ];
}

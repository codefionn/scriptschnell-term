{
  lib,
  makeRustPlatform,
  rustToolchain,
  pkg-config,
  patchelf,
  fontconfig,
  freetype,
  libGL,
  libffi,
  libxkbcommon,
  vulkan-loader,
  wayland,
  libx11,
  libxcb,
  libxcursor,
  libxi,
  libxrandr,
}:

let
  rustPlatform = makeRustPlatform {
    cargo = rustToolchain;
    rustc = rustToolchain;
  };
  root = ../.;
  workspace = lib.importTOML ../Cargo.toml;

  # winit and wgpu open these with dlopen, and Oximond opens libffi on the
  # first foreign call from Lua, so they are not in the binary's needed
  # libraries. The app finds them through its RUNPATH.
  runtimeLibraries = [
    libGL
    libffi
    libxkbcommon
    vulkan-loader
    wayland
    libx11
    libxcb
    libxcursor
    libxi
    libxrandr
  ];
in
rustPlatform.buildRustPackage {
  pname = "scriptschnell-term";
  inherit (workspace.workspace.package) version;

  # Only what the build reads, so a change to docs or CI does not rebuild.
  # about.rs embeds the changelog and licenses, bundle.rs the shipped extensions.
  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../CHANGELOG.md
      ../LICENSE-APACHE
      ../LICENSE-MIT
      ../crates
      ../extensions
      ../packaging/linux
    ];
  };

  cargoLock = {
    lockFile = ../Cargo.lock;
    # schnellui and oximond are git dependencies that move often. Cargo.lock pins
    # their commits, so Nix fetches those without a hash to keep up to date here.
    allowBuiltinFetchGit = true;
  };

  nativeBuildInputs = [
    pkg-config
    patchelf
  ];
  buildInputs = [
    fontconfig
    freetype
    # Oximond compiles against ffi.h and reads the soname of libffi.so.
    libffi
  ];

  # Oximond's build script asks `cc -print-file-name=libffi.so` for the library,
  # which searches LIBRARY_PATH but not the -L flags the Nix compiler wrapper adds.
  env.LIBRARY_PATH = lib.makeLibraryPath [ libffi ];

  # The tests start bubblewrap sandboxes, Mercurial and network servers, which
  # the Nix build sandbox does not allow. CI runs them.
  doCheck = false;

  postInstall = ''
    # The Windows sandbox launcher only exits on other platforms, and the release
    # signing tool is for CI.
    rm -f $out/bin/scriptschnell-term-sandbox $out/bin/scriptschnell-term-release

    install -Dm644 packaging/linux/scriptschnell-term.desktop -t $out/share/applications
    install -Dm644 packaging/linux/scriptschnell-term.svg -t $out/share/icons/hicolor/scalable/apps
    install -Dm644 packaging/linux/scriptschnell-term.png -t $out/share/icons/hicolor/256x256/apps
  '';

  # After the fixup phase shrank the RUNPATH, which would drop these again.
  # A RUNPATH instead of a wrapper script keeps current_exe() in $out/bin, where
  # the app looks for scriptschnell-term-remote and scriptschnell-term-proxy-bridge.
  postFixup = ''
    patchelf --add-rpath ${lib.makeLibraryPath runtimeLibraries} $out/bin/scriptschnell-term
  '';

  passthru = { inherit runtimeLibraries; };

  meta = {
    description = "GPU-rendered terminal and browser panels organized by VCS project and worktree";
    homepage = "https://github.com/codefionn/scriptschnell-term";
    license = with lib.licenses; [
      mit
      asl20
    ];
    mainProgram = "scriptschnell-term";
    platforms = lib.platforms.linux;
  };
}

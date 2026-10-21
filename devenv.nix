# The only environment of the project. A tool that is not here does not
# exist for the project.
{ pkgs, inputs, config, lib, ... }:

let
  # The shell also opens on macOS, for edits, `just check` and the tests.
  # The parts below that only exist on Linux are on Linux only. The
  # targets of the project stay Linux.
  isLinux = pkgs.stdenv.isLinux;

  dagger = inputs.dagger.packages.${pkgs.stdenv.hostPlatform.system}.dagger;

  # Put only the versioned interpreter names on PATH, so that the four
  # builds do not collide. python3 is 3.13, the newest version that
  # Plone 6.1 supports.
  pythons = pkgs.runCommand "zoe-pythons" { } ''
    mkdir -p $out/bin
    ln -s ${pkgs.python312}/bin/python3.12 $out/bin/python3.12
    ln -s ${pkgs.python313}/bin/python3.13 $out/bin/python3.13
    ln -s ${pkgs.python314}/bin/python3.14 $out/bin/python3.14
    ln -s ${pkgs.python314FreeThreading}/bin/python3.14t $out/bin/python3.14t
    ln -s ${pkgs.python313}/bin/python3.13 $out/bin/python3
  '';

  # The WebDriver server of the Loupe end-to-end tests. nixpkgs does not
  # package it, so it builds from its crate with the pinned toolchain.
  rust = config.languages.rust.toolchainPackage;
  tauri-driver = (pkgs.makeRustPlatform { cargo = rust; rustc = rust; }).buildRustPackage rec {
    pname = "tauri-driver";
    version = "2.0.6";
    src = pkgs.fetchCrate {
      inherit pname version;
      hash = "sha256-fTCkEs4NLBW0khaHL4jpVNkrbQg22YPsRMjfJNqnCWA=";
    };
    cargoHash = "sha256-MThAcU+U8PyBGauh3dy7ZRvRX9INmOEeghIlQEGLAPs=";
  };

  # The C compiler of the musl target. The C code of zstd-sys and blake3
  # compiles against the musl headers, not against the glibc headers of
  # the shell.
  musl-cc = pkgs.pkgsCross.musl64.stdenv.cc;

  # Put only setpriv on PATH. The other tools of util-linux, such as mount
  # and su, must not hide the tools of the host. CI runs the conformance
  # suites through setpriv.
  setpriv = pkgs.runCommand "zoe-setpriv" { } ''
    mkdir -p $out/bin
    ln -s ${pkgs.lib.getExe' pkgs.util-linuxMinimal "setpriv"} $out/bin/setpriv
  '';
in
{
  languages.rust = {
    enable = true;
    channel = "stable";
    version = "1.99.0";
    # `just tsan` rebuilds the standard library from rust-src.
    components = [ "rustc" "cargo" "clippy" "rustfmt" "rust-src" ];
    # `just release binary` builds the static `zoe` binary for musl.
    targets = [ "x86_64-unknown-linux-musl" ];
  };

  packages = [
    pythons
    pkgs.uv
    pkgs.ruff
    pkgs.just
    pkgs.mdbook
    pkgs.nodejs_24
    pkgs.git
    dagger
  ] ++ lib.optionals isLinux [
    setpriv
  ] ++ [
    # Libraries that the Plone instances need when buildout builds an
    # egg from source.
    pkgs.pkg-config
    pkgs.zlib
    pkgs.libffi
    pkgs.libxml2
    pkgs.libxslt
    pkgs.libjpeg
    pkgs.openssl
  ];

  env = {
    # uv uses the interpreters above and never downloads one.
    UV_PYTHON_DOWNLOADS = "never";
    UV_PYTHON_PREFERENCE = "only-system";
  } // lib.optionalAttrs isLinux {
    # The static `zoe` binary: cc compiles its C code and links it with
    # the musl toolchain.
    CC_x86_64_unknown_linux_musl = "${musl-cc}/bin/${musl-cc.targetPrefix}cc";
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = "${musl-cc}/bin/${musl-cc.targetPrefix}cc";

    # The Plone suites switch to Latin-1 locales. The interpreters find
    # every locale in this archive. macOS has its own locales.
    LOCALE_ARCHIVE = "${pkgs.glibcLocales}/lib/locale/locale-archive";

    # The C++ runtime of the shell. The wheels of greenlet and gevent, which
    # Locust loads, link libstdc++.so.6, and the interpreters of the shell
    # do not find it. `just bench plone` puts it on LD_LIBRARY_PATH. The
    # shell does not, so that the tools of the host keep their own
    # libraries.
    ZOE_CXX_LIBS = lib.makeLibraryPath [ pkgs.stdenv.cc.cc.lib ];
  };

  # The link check of `just docs`. It reads SUMMARY.md and the pages that
  # it lists. It fails on a link to a file that does not exist, and on a
  # link to a .md file of the book that SUMMARY.md does not list. It skips
  # code, anchors and links with a scheme.
  scripts.docs-linkcheck = {
    description = "Check the links of the book";
    package = pkgs.python313;
    exec = ''
      import re
      import sys
      from pathlib import Path
      from urllib.parse import unquote

      root = Path(sys.argv[1] if len(sys.argv) > 1 else "docs").resolve()
      summary = root / "SUMMARY.md"
      fence = re.compile(r"^[ \t]*(```+|~~~+).*?^[ \t]*\1", re.M | re.S)
      code = re.compile(r"(`+).+?\1", re.S)
      inline = re.compile(r"\]\(\s*<?([^\s)>]+)")
      definition = re.compile(r"^ {0,3}\[[^\]]+\]:\s*<?([^\s>]+)", re.M)
      scheme = re.compile(r"[A-Za-z][A-Za-z0-9+.-]*:")


      def links(page):
          text = code.sub("", fence.sub("", page.read_text()))
          for target in inline.findall(text) + definition.findall(text):
              if not target.startswith("#") and not scheme.match(target):
                  yield target, unquote(target.split("#", 1)[0])


      listed = {(root / path).resolve() for _, path in links(summary)}
      errors = []
      for page in [summary, *sorted(listed)]:
          if not page.is_file():
              errors.append(f"SUMMARY.md: {page.relative_to(root)}: no such file")
              continue
          for target, path in links(page):
              where = f"{page.relative_to(root)}: {target}"
              dest = (page.parent / path).resolve()
              if not dest.exists():
                  errors.append(f"{where}: no such file")
              elif dest.suffix == ".md" and dest.is_relative_to(root) and dest not in listed:
                  errors.append(f"{where}: SUMMARY.md does not list it")
      for error in errors:
          print(error, file=sys.stderr)
      print(f"docs-linkcheck: {len(errors)} broken links", file=sys.stderr)
      sys.exit(1 if errors else 0)
    '';
  };

  # Heap and CPU profiles for the spikes. Enter the profile with
  # `devenv --profile profiling shell`. The default shell and CI do not
  # fetch it, because heaptrack brings Qt. perf needs a
  # `kernel.perf_event_paranoid` of 2 or less on the host. Both tools are
  # Linux only, so the profile is empty on macOS.
  profiles.profiling.module = {
    packages = lib.optionals isLinux [
      pkgs.heaptrack
      pkgs.perf
    ];
  };

  # Loupe, the Tauri app, and its end-to-end tests. Enter the profile with
  # `devenv --profile loupe shell`. The default shell and CI do not build
  # it.
  profiles.loupe.module = {
    # On macOS, a Tauri app links the WebKit of the system, and tauri-driver
    # does not run. The profile then gives only the Tauri CLI.
    packages = [
      pkgs.cargo-tauri
    ] ++ lib.optionals isLinux [
      # The libraries that a Tauri app links on Linux.
      pkgs.webkitgtk_4_1
      pkgs.libsoup_3
      pkgs.gtk3
      pkgs.glib
      pkgs.cairo
      pkgs.pango
      pkgs.gdk-pixbuf
      pkgs.at-spi2-atk
      pkgs.harfbuzz
      pkgs.librsvg
      # The end-to-end tests: webkitgtk gives WebKitWebDriver, which
      # tauri-driver drives, and xvfb-run gives them a display.
      tauri-driver
      pkgs.xvfb-run
    ];

    env = lib.optionalAttrs isLinux {
      # The GTK file dialog of Loupe reads these GSettings schemas.
      XDG_DATA_DIRS = "${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk3}/share/gsettings-schemas/${pkgs.gtk3.name}";
      # WebKitGTK needs EGL. The EGL of the shell finds no driver of the
      # host, so it uses Mesa. Without it, the web process of Loupe stops
      # at start.
      __EGL_VENDOR_LIBRARY_FILENAMES = "${pkgs.mesa}/share/glvnd/egl_vendor.d/50_mesa.json";
    };
  };
}

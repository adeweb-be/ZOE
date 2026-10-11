# The only environment of the project. A tool that is not here does not
# exist for the project.
{ pkgs, inputs, ... }:

let
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
  # package it.
  tauri-driver = pkgs.rustPlatform.buildRustPackage rec {
    pname = "tauri-driver";
    version = "2.0.6";
    src = pkgs.fetchCrate {
      inherit pname version;
      hash = "sha256-fTCkEs4NLBW0khaHL4jpVNkrbQg22YPsRMjfJNqnCWA=";
    };
    cargoHash = "sha256-MThAcU+U8PyBGauh3dy7ZRvRX9INmOEeghIlQEGLAPs=";
  };
in
{
  languages.rust = {
    enable = true;
    channel = "stable";
    version = "1.99.0";
    # `just tsan` rebuilds the standard library from rust-src.
    components = [ "rustc" "cargo" "clippy" "rustfmt" "rust-src" ];
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

    # Libraries that the Plone instances need when buildout builds an
    # egg from source.
    pkgs.pkg-config
    pkgs.zlib
    pkgs.libffi
    pkgs.libxml2
    pkgs.libxslt
    pkgs.libjpeg
    pkgs.openssl

    # Loupe: the Tauri CLI and the libraries that a Tauri app links on
    # Linux. webkitgtk also gives WebKitWebDriver, which tauri-driver
    # drives. xvfb-run gives the end-to-end tests a display.
    pkgs.cargo-tauri
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
    pkgs.xvfb-run
    tauri-driver
  ];

  env = {
    # uv uses the interpreters above and never downloads one.
    UV_PYTHON_DOWNLOADS = "never";
    UV_PYTHON_PREFERENCE = "only-system";
    # The GTK file dialog of Loupe reads these GSettings schemas.
    XDG_DATA_DIRS = "${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk3}/share/gsettings-schemas/${pkgs.gtk3.name}";
    # WebKitGTK needs EGL. The EGL of the shell finds no driver of the host,
    # so it uses Mesa from the shell. Without it, the web process of Loupe
    # stops at start.
    __EGL_VENDOR_LIBRARY_FILENAMES = "${pkgs.mesa}/share/glvnd/egl_vendor.d/50_mesa.json";
  };
}

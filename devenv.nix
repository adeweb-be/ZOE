# The only environment of the project. A tool that is not here does not
# exist for the project. This file is a red zone.
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
in
{
  languages.rust = {
    enable = true;
    channel = "stable";
    version = "1.99.0";
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
  ];

  env = {
    # uv uses the interpreters above and never downloads one.
    UV_PYTHON_DOWNLOADS = "never";
    UV_PYTHON_PREFERENCE = "only-system";
  };
}

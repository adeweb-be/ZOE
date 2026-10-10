from typing import Annotated

import dagger
from dagger import DefaultPath, Doc, Ignore, dag, function, object_type

NIX_IMAGE = "nixos/nix:2.35.2"

# The devenv release that reads devenv.yaml and devenv.lock. Keep it equal
# to the devenv version of the developers.
DEVENV = "github:cachix/devenv/v1.10"

NIX_CONF = """\
experimental-features = nix-command flakes
sandbox = false
filter-syscalls = false
accept-flake-config = true
extra-substituters = https://devenv.cachix.org
extra-trusted-public-keys = devenv.cachix.org-1:w1cLUi8dv3hnoSPGAuibQv+f9TZLr6cv/Hm9XgU50cw=
"""

# Local state that must not go into the container.
SOURCE_IGNORE = [
    ".git",
    ".devenv",
    ".devenv.flake.nix",
    ".direnv",
    "target",
    "**/__pycache__",
    "python/.venv*",
    "python/zoedb/src/zoe/*.so",
    "ci/.venv",
    "ci/sdk",
    "plone/.installed.cfg",
    "plone/bin",
    "plone/develop-eggs",
    "plone/eggs",
    "plone/parts",
    "plone/var",
]

Source = Annotated[
    dagger.Directory,
    DefaultPath("/"),
    Ignore(SOURCE_IGNORE),
    Doc("The repository"),
]


@object_type
class Ci:
    """The CI functions. GitHub Actions workflows call them."""

    def devenv(self, source: dagger.Directory) -> dagger.Container:
        """Return a container with devenv and the repository in /src."""
        base = dag.container().from_(NIX_IMAGE)
        return (
            base.with_mounted_cache(
                "/nix",
                dag.cache_volume("zoe-nix"),
                source=base.directory("/nix"),
                sharing=dagger.CacheSharingMode.LOCKED,
            )
            .with_new_file("/etc/nix/nix.conf", NIX_CONF)
            .with_exec(["nix", "profile", "add", DEVENV])
            .with_mounted_cache(
                "/root/.cargo/registry", dag.cache_volume("zoe-cargo-registry")
            )
            .with_mounted_cache("/root/.cache/uv", dag.cache_volume("zoe-uv"))
            # The uv cache is a volume, so uv cannot hardlink from it.
            .with_env_variable("UV_LINK_MODE", "copy")
            .with_directory("/src", source)
            .with_workdir("/src")
        )

    def recipe(
        self, source: dagger.Directory, name: str, cache_target: bool = True
    ) -> dagger.Container:
        """Run one just recipe inside the devenv shell."""
        ctr = self.devenv(source)
        if cache_target:
            ctr = ctr.with_mounted_cache("/src/target", dag.cache_volume("zoe-target"))
        return ctr.with_exec(["devenv", "shell", "--", "just", name])

    @function
    async def check(self, source: Source) -> str:
        """Check the format and run clippy and the Python lints."""
        return await self.recipe(source, "check").stdout()

    @function
    async def test(self, source: Source) -> str:
        """Run the Rust tests and the Python tests on every interpreter."""
        return await self.recipe(source, "test").stdout()

    @function
    def docs(self, source: Source) -> dagger.Directory:
        """Build the documentation site and return it."""
        # The book goes to target/book, so target must not be a cache volume here.
        return self.recipe(source, "docs", cache_target=False).directory(
            "/src/target/book"
        )

    @function
    async def conformance(self, source: Source) -> str:
        """Run the ZODB and ZEO conformance suites. Not built yet."""
        return await self.recipe(source, "conformance").stdout()

    @function
    async def plone(self, source: Source) -> str:
        """Build the Plone instances and run the Plone suites. Not built yet."""
        return await self.recipe(source, "plone").stdout()

    @function
    async def bench(self, source: Source) -> str:
        """Run the benchmarks. Not built yet."""
        return await self.recipe(source, "bench").stdout()

    @function
    async def crash(self, source: Source) -> str:
        """Run the crash-consistency harness. Not built yet."""
        return (
            await self.devenv(source)
            .with_exec(
                [
                    "echo",
                    "crash: runs the Vfs simulation, the kill loop and the power-loss emulation. Not built yet.",
                ]
            )
            .stdout()
        )

    @function
    async def release(self, source: Source) -> str:
        """Build the release artifacts. Not built yet."""
        return await self.recipe(source, "release").stdout()

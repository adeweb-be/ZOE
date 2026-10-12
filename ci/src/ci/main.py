import tomllib
from typing import Annotated

import dagger
from dagger import DefaultPath, Doc, Ignore, dag, function, object_type

NIX_IMAGE = "nixos/nix:2.35.2"

# The PostgreSQL of the RelStorage baseline.
POSTGRES_IMAGE = "postgres:17.10-alpine"

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

# The user of the server image: a numeric id that is not root. A runtime
# can check a numeric id without an /etc/passwd in the image.
IMAGE_USER = "65532:65532"

# The store directory in the server image. The image user owns it.
IMAGE_STORE = "/var/lib/zoe"

# Local state that must not go into the container.
SOURCE_IGNORE = [
    ".git",
    ".devenv",
    ".devenv.flake.nix",
    ".direnv",
    "target",
    "**/__pycache__",
    "**/.pytest_cache",
    "**/.ruff_cache",
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
        self,
        source: dagger.Directory,
        name: str,
        args: tuple[str, ...] = (),
        cache_target: bool = True,
        privileged: bool = False,
    ) -> dagger.Container:
        """Run one just recipe inside the devenv shell."""
        ctr = self.devenv(source)
        if cache_target:
            ctr = ctr.with_mounted_cache("/src/target", dag.cache_volume("zoe-target"))
        return ctr.with_exec(
            ["devenv", "shell", "--", "just", name, *args],
            insecure_root_capabilities=privileged,
        )

    async def workspace(self, source: dagger.Directory) -> dict:
        """Return the workspace package table of Cargo.toml."""
        cargo = tomllib.loads(await source.file("Cargo.toml").contents())
        return cargo["workspace"]["package"]

    @function
    async def check(self, source: Source) -> str:
        """Check the format and run clippy and the Python lints."""
        return await self.recipe(source, "check").stdout()

    @function
    async def test(self, source: Source) -> str:
        """Run the Rust tests and the Python tests on every interpreter."""
        return await self.recipe(source, "test").stdout()

    @function
    async def tsan(self, source: Source) -> str:
        """Run the zoe-core tests under ThreadSanitizer and check the canary race."""
        # On a host with high-entropy ASLR, ThreadSanitizer turns off ASLR with
        # the personality call and runs the test again. The seccomp filter of a
        # normal container refuses this call. So this exec runs with all root
        # capabilities and no seccomp filter.
        return await self.recipe(source, "tsan", privileged=True).stdout()

    @function
    def docs(self, source: Source) -> dagger.Directory:
        """Build the documentation site and return it."""
        # The book goes to target/book, so target must not be a cache volume here.
        return self.recipe(source, "docs", cache_target=False).directory(
            "/src/target/book"
        )

    @function
    async def conformance(self, source: Source) -> str:
        """Run the ZODB conformance suites in-process. The ZEO suites are not built."""
        return await self.recipe(source, "conformance").stdout()

    @function
    async def plone(self, source: Source) -> str:
        """Build the Plone instances and run the Plone suites. Not built yet."""
        return await self.recipe(source, "plone").stdout()

    @function
    def bench(
        self,
        source: Source,
        quick: Annotated[
            bool, Doc("Run the quick set instead of the full set")
        ] = False,
        commit: Annotated[str, Doc("The commit to record in the results")] = "",
    ) -> dagger.Directory:
        """Run the harness tests and the zodbshootout baseline.

        Return the results tree, in the layout of the results repository.
        """
        postgres = (
            dag.container()
            .from_(POSTGRES_IMAGE)
            # The service is reachable only inside this pipeline.
            .with_env_variable("POSTGRES_HOST_AUTH_METHOD", "trust")
            .with_env_variable("POSTGRES_DB", "zoe_bench")
            .with_exposed_port(5432)
            .as_service()
        )
        run = ["devenv", "shell", "--", "just", "bench", "run", "--output", "/results"]
        return (
            self.devenv(source)
            .with_mounted_cache("/src/target", dag.cache_volume("zoe-target"))
            .with_service_binding("postgres", postgres)
            .with_env_variable(
                "ZOE_BENCH_PG_DSN", "host=postgres dbname=zoe_bench user=postgres"
            )
            .with_env_variable("ZOE_BENCH_COMMIT", commit)
            .with_exec(["devenv", "shell", "--", "just", "bench", "test"])
            .with_exec(run + (["--quick"] if quick else []))
            .directory("/results")
        )

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
    def zoe_binary(self, source: Source) -> dagger.File:
        """Build the static `zoe` binary, with `serve`, for Linux x86-64."""
        # The recipe copies the binary out of the target cache volume.
        return self.recipe(source, "release", ("binary", "/dist")).file("/dist/zoe")

    @function
    async def server_image(self, source: Source) -> dagger.Container:
        """Build the server image: the static `zoe` binary and nothing else.

        `export` writes the image as an OCI tarball. The address of the image
        is ghcr.io/adeweb-be/zoe, and its tags are the release versions.
        """
        package = await self.workspace(source)
        # The image starts empty: no shell, no package manager, no
        # interpreter. The binary needs no C library.
        return (
            dag.container(platform=dagger.Platform("linux/amd64"))
            .with_file("/usr/local/bin/zoe", self.zoe_binary(source), permissions=0o755)
            .with_directory(IMAGE_STORE, dag.directory(), owner=IMAGE_USER)
            .with_env_variable("ZOE_PATH", IMAGE_STORE)
            # The runtime of the container exposes the ports, so the server
            # listens on all the addresses of the container.
            .with_env_variable("ZOE_LISTEN", "0.0.0.0:8100")
            .with_user(IMAGE_USER)
            .with_entrypoint(["/usr/local/bin/zoe"])
            .with_default_args(["serve"])
            .with_exposed_port(8100, description="ZEO")
            .with_exposed_port(8101, description="admin")
            .with_label("org.opencontainers.image.version", package["version"])
            .with_label("org.opencontainers.image.source", package["repository"])
        )

    @function
    async def release(
        self,
        source: Source,
        tag: Annotated[
            str, Doc("The git tag of the release. It must be v and the version.")
        ] = "",
    ) -> dagger.Directory:
        """Build the release artifacts: the static binary and the server image.

        The version bump, the wheels and the Helm chart are not built yet.
        """
        version = (await self.workspace(source))["version"]
        # The tag of the image is the version. So the git tag must name the
        # version that the build gives to the binary and the image.
        if tag and tag != f"v{version}":
            raise ValueError(f"the tag {tag} is not v{version}, the version")
        image = await self.server_image(source)
        return (
            dag.directory()
            .with_file(
                f"zoe-{version}-x86_64-unknown-linux-musl", self.zoe_binary(source)
            )
            .with_file(f"zoe-{version}-image.tar", image.as_tarball())
        )

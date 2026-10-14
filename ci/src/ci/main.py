import random
import tomllib
from typing import Annotated

import dagger
from dagger import DefaultPath, Doc, Ignore, dag, function, object_type

NIX_IMAGE = "nixos/nix:2.35.2"

# The PostgreSQL of the RelStorage baseline.
POSTGRES_IMAGE = "postgres:17.10-alpine"

# The devenv release that reads devenv.yaml and devenv.lock. Keep it equal
# to require_version in devenv.yaml.
DEVENV = "github:cachix/devenv/v2.4.0"

NIX_CONF = """\
experimental-features = nix-command flakes
sandbox = false
filter-syscalls = false
accept-flake-config = true
extra-substituters = https://devenv.cachix.org
extra-trusted-public-keys = devenv.cachix.org-1:w1cLUi8dv3hnoSPGAuibQv+f9TZLr6cv/Hm9XgU50cw=
!include /etc/nix/access-tokens.conf
"""

# The user of the server image: a numeric id that is not root. A runtime
# can check a numeric id without an /etc/passwd in the image.
IMAGE_USER = "65532:65532"

# The store directory in the server image. The image user owns it.
IMAGE_STORE = "/var/lib/zoe"

# The capabilities that let root ignore the mode of a file. The containers
# run as root, but some tests expect a read-only file to refuse a write.
FILE_MODE_CAPS = "-dac_override,-dac_read_search"

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

    github_token: Annotated[
        dagger.Secret | None,
        Doc("A GitHub token. Nix sends it with its requests to the GitHub API"),
    ] = None

    async def devenv(self, source: dagger.Directory) -> dagger.Container:
        """Return a container with devenv and the repository in /src."""
        base = dag.container().from_(NIX_IMAGE)
        ctr = base.with_mounted_cache(
            "/nix",
            dag.cache_volume("zoe-nix"),
            source=base.directory("/nix"),
            sharing=dagger.CacheSharingMode.LOCKED,
        ).with_new_file("/etc/nix/nix.conf", NIX_CONF)
        # Nix fetches the github: inputs through the GitHub API. Without a
        # token, the API allows few requests for each address, and hosted
        # runners share their addresses. Nix ignores a missing include file.
        if self.github_token is not None:
            token = await self.github_token.plaintext()
            tokens = dag.set_secret(
                "nix-access-tokens", f"access-tokens = github.com={token}\n"
            )
            ctr = ctr.with_mounted_secret("/etc/nix/access-tokens.conf", tokens)
        return (
            ctr.with_exec(["nix", "profile", "add", DEVENV])
            .with_mounted_cache(
                "/root/.cargo/registry", dag.cache_volume("zoe-cargo-registry")
            )
            .with_mounted_cache("/root/.cache/uv", dag.cache_volume("zoe-uv"))
            # The uv cache is a volume, so uv cannot hardlink from it.
            .with_env_variable("UV_LINK_MODE", "copy")
            .with_directory("/src", source)
            .with_workdir("/src")
        )

    async def recipe(
        self,
        source: dagger.Directory,
        name: str,
        args: tuple[str, ...] = (),
        cache_target: bool = True,
        privileged: bool = False,
        file_modes: bool = False,
    ) -> dagger.Container:
        """Run one just recipe inside the devenv shell."""
        ctr = await self.devenv(source)
        if cache_target:
            ctr = ctr.with_mounted_cache("/src/target", dag.cache_volume("zoe-target"))
        command = ["just", name, *args]
        if file_modes:
            # Root keeps its other capabilities. Without these two, the
            # kernel applies the mode of each file to root too.
            command = [
                "setpriv",
                f"--inh-caps={FILE_MODE_CAPS}",
                f"--bounding-set={FILE_MODE_CAPS}",
                *command,
            ]
        return ctr.with_exec(
            ["devenv", "shell", "--", *command],
            insecure_root_capabilities=privileged,
        )

    async def workspace(self, source: dagger.Directory) -> dict:
        """Return the workspace package table of Cargo.toml."""
        cargo = tomllib.loads(await source.file("Cargo.toml").contents())
        return cargo["workspace"]["package"]

    @function
    async def check(self, source: Source) -> str:
        """Check the format and run clippy and the Python lints."""
        ctr = await self.recipe(source, "check")
        return await ctr.stdout()

    @function
    async def test(self, source: Source) -> str:
        """Run the Rust tests and the Python tests on every interpreter."""
        ctr = await self.recipe(source, "test")
        return await ctr.stdout()

    @function
    async def tsan(self, source: Source) -> str:
        """Run the zoe-core tests under ThreadSanitizer and check the canary race."""
        # On a host with high-entropy ASLR, ThreadSanitizer turns off ASLR with
        # the personality call and runs the test again. The seccomp filter of a
        # normal container refuses this call. So this exec runs with all root
        # capabilities and no seccomp filter.
        ctr = await self.recipe(source, "tsan", privileged=True)
        return await ctr.stdout()

    @function
    async def fuzz(
        self,
        source: Source,
        seconds: Annotated[
            int, Doc("The time of the whole run, split between the targets")
        ] = 3600,
    ) -> str:
        """Run the fuzz targets of zoe serve, one hour by default."""
        # The inputs that libFuzzer finds stay in the target cache volume, so
        # the next run starts from them.
        ctr = await self.recipe(source, "fuzz", (str(seconds),))
        return await ctr.stdout()

    @function
    async def docs(self, source: Source) -> dagger.Directory:
        """Build the documentation site and return it."""
        # The book goes to target/book, so target must not be a cache volume here.
        ctr = await self.recipe(source, "docs", cache_target=False)
        return ctr.directory("/src/target/book")

    @function
    async def conformance(self, source: Source) -> str:
        """Run the ZODB suites in-process and the ZEO suites against zoe serve."""
        # The blob suite of ZODB opens a committed blob for writing and
        # expects a PermissionError. Root gets none unless file modes apply.
        ctr = await self.recipe(source, "conformance", file_modes=True)
        return await ctr.stdout()

    @function
    async def plone(self, source: Source) -> str:
        """Run the Plone suites on ZOE in-process, then through zoe serve."""
        # The second run starts from the first one, so it reuses its eggs.
        zoe = await self.recipe(source, "plone", ("zoe", "suites"))
        zeoclient = zoe.with_exec(
            ["devenv", "shell", "--", "just", "plone", "zeoclient", "suites"]
        )
        return await zoe.stdout() + await zeoclient.stdout()

    @function
    async def bench(
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
        ctr = await self.devenv(source)
        return (
            ctr.with_mounted_cache("/src/target", dag.cache_volume("zoe-target"))
            .with_service_binding("postgres", postgres)
            .with_env_variable(
                "ZOE_BENCH_PG_DSN", "host=postgres dbname=zoe_bench user=postgres"
            )
            .with_env_variable("ZOE_BENCH_COMMIT", commit)
            .with_exec(["devenv", "shell", "--", "just", "bench", "test"])
            .with_exec(run + (["--quick"] if quick else []))
            .directory("/results")
        )

    @function(cache="never")
    async def crash(
        self,
        source: Source,
        runs: Annotated[int, Doc("The rounds of the kill loop")] = 10_000,
        seed: Annotated[
            int | None, Doc("The seed of the kill loop. The default is a random seed")
        ] = None,
        seeds: Annotated[
            int, Doc("The seeds of the kill loop under the simulator")
        ] = 10_000,
    ) -> str:
        """Run the process kill loop, then the kill loop under the simulator.

        The process kill loop runs 10,000 rounds against zoe serve by
        default. The output gives the seed. The traceback of a failure
        shows the round. 10,000 rounds need about 3 GB in TMPDIR. The kill
        loop under the simulator runs `ZOE_SIM_SEEDS=10000` by default,
        with power-loss crash images at each phase of a commit. A failing
        seed prints `ZOE_SIM_SEED=<seed>`.
        """
        # The cache policy "never" makes each call run this body. A new seed
        # for each call also gives a new exec, so the exec cache misses too.
        if seed is None:
            seed = random.randrange(1 << 30)
        args = (str(runs), str(seed), str(seeds))
        ctr = await self.recipe(source, "crash", args)
        return await ctr.stdout()

    @function
    async def zoe_binary(self, source: Source) -> dagger.File:
        """Build the static `zoe` binary, with `serve`, for Linux x86-64."""
        # The build uses no shared target cache. Cargo finds changed files
        # by their mtimes, so a cache that another checkout filled can give
        # objects of other code.
        ctr = await self.recipe(
            source, "release", ("binary", "/dist"), cache_target=False
        )
        return ctr.file("/dist/zoe")

    @function
    async def server_image(self, source: Source) -> dagger.Container:
        """Build the server image: the static `zoe` binary and nothing else.

        `export` writes the image as an OCI tarball. The address of the image
        is ghcr.io/adeweb-be/zoe, and its tags are the release versions.
        """
        package = await self.workspace(source)
        binary = await self.zoe_binary(source)
        # The image starts empty: no shell, no package manager, no
        # interpreter. The binary needs no C library.
        return (
            dag.container(platform=dagger.Platform("linux/amd64"))
            .with_file("/usr/local/bin/zoe", binary, permissions=0o755)
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
        binary = await self.zoe_binary(source)
        return (
            dag.directory()
            .with_file(f"zoe-{version}-x86_64-unknown-linux-musl", binary)
            .with_file(f"zoe-{version}-image.tar", image.as_tarball())
        )

# Install

No release of ZOE is published yet. The `zoedb` wheel is not on PyPI, and the server image is not on `ghcr.io`. You build each part from the source, in the development shell of the repository.

## The parts

| Part | What it holds | Where you install it |
| --- | --- | --- |
| the `zoedb` wheel | the package `zoe`: the in-process storage `zoe.Storage`, the `<zoe>` and `<zoeclient>` sections of `zope.conf`, the Plone integration `zoe.plone`, and the console script `zoe` | the Python environment of each Zope or Plone instance that opens a store in-process or connects with `<zoeclient>` |
| the static `zoe` binary | every subcommand of `zoe`, with `zoe serve` and `zoe mcp` | the host that runs the server, and each host where you run `zoe mcp` |
| the server image | the static binary in an empty image, for Kubernetes or a container runtime | your registry |

A stock ZEO client of `zoe serve` needs no part of ZOE. It needs the Python package `msgpack`, version 1 or later (see [Connect clients](clients.md)).

The console script `zoe` of the wheel runs the same code as the binary, without `serve` and `mcp`. In the wheel, `zoe serve` and `zoe mcp` fail with exit code 1, and the message names the `zoe` binary.

## Get the source and the shell

The build runs in the shell of the repository. [devenv](https://devenv.sh) gives the shell: the Rust toolchain, CPython 3.12 to 3.14, uv, just and Dagger, at pinned versions. Install devenv first.

```sh
git clone https://github.com/adeweb-be/ZOE.git
cd ZOE
devenv shell
```

Run the commands below in that shell, at the root of the repository.

## The wheel

`just release wheel DIR VERSION...` builds the wheel into `DIR`, for each CPython version that you give, 3.13 by default:

```sh
just release wheel target/wheels 3.13
```

The wheel holds an extension module in Rust, so it is for one CPython version and one platform. Its name tells which, for example `zoedb-0.0.0-cp313-cp313-linux_x86_64.whl`. Build one wheel for each version that you use: 3.12, 3.13, 3.14 or 3.14t, such as `just release wheel target/wheels 3.12 3.13`. Build it on the platform where it runs: Linux x86-64, or macOS on Apple silicon.

Install the wheel into the Python environment of the instance, with the extras that you need. Use the name of the file that the recipe wrote:

```sh
pip install "target/wheels/zoedb-0.0.0-cp313-cp313-linux_x86_64.whl[plone]"
```

| Extra | Adds | Use it for |
| --- | --- | --- |
| none | `ZConfig` and `ZODB` | a Zope application, or a script, on `zoe.Storage` or `<zoeclient>` |
| `plone` | `Products.CMFPlone` 6.1 or later and `msgpack`, for `zoe.plone`, which every wheel holds | a Plone site |
| `uri` | `zodburi` 3 or later | the `zoe://` URI (see [Run in-process](in-process.md#the-zoe-uri)) |

[Compatibility](../reference/compatibility.md) gives the versions of CPython, ZODB, ZEO and Plone. On the free-threaded CPython 3.14t, read [Known issues](known-issues.md) first.

For a buildout, see [Run in-process](in-process.md#in-a-buildout): buildout finds no `zoedb` on PyPI.

## The binary and the image

`just release` builds the static `zoe` binary and the server image into `target/dist`:

```sh
just release
```

It writes two files, with the version of the workspace in their names:

| File | What it is |
| --- | --- |
| `zoe-VERSION-x86_64-unknown-linux-musl` | the static `zoe` binary for Linux x86-64. It needs no C library and no Python. |
| `zoe-VERSION-image.tar` | the server image, as an OCI tarball |

`just release` runs the build in Dagger, so it needs a container runtime for the Dagger engine. To build the binary only, in the shell, run `just release binary DIR`. It copies the binary to `DIR`.

Both recipes need Linux x86-64. No recipe builds a release binary for macOS yet.

### Install the binary on a host

Copy the binary to `/usr/local/bin/zoe` and make it executable:

```sh
install -m 0755 target/dist/zoe-0.0.0-x86_64-unknown-linux-musl /usr/local/bin/zoe
zoe --version
```

[Deploy on a host](deploy-host.md) gives the systemd unit and the supervisor program.

### Push the image to your registry

The image holds only the binary, at `/usr/local/bin/zoe`. Its entry point is `zoe`, and its default argument is `serve`. It runs as the user `65532`, which is not root. It sets `ZOE_PATH=/var/lib/zoe` and `ZOE_LISTEN=0.0.0.0:8100`, and exposes the ZEO port 8100 and the admin port 8101.

Load the tarball with your container tool, tag it and push it to a registry that your cluster reads. For example, with Docker:

```sh
docker load --input target/dist/zoe-0.0.0-image.tar
docker tag IMAGE_ID registry.example.org/zoe:0.0.0
docker push registry.example.org/zoe:0.0.0
```

`docker load` writes the id of the image. [Deploy on Kubernetes](deploy-kubernetes.md) uses the image.

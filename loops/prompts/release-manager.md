# Role: release manager

You cut a release when the human tags one.

## Inputs

- The tag, and the merged cards since the previous tag.

## Do

1. Write the changelog from the merged cards.
2. Bump the versions in lockstep: the Cargo workspace and the `zoedb` wheel together.
3. Build the wheels, the static binary, the image and the Helm chart, and sign the binaries.
4. Run the install smoke test. If it fails, delete the tag and open a card.
5. Publish the artifacts.

## Never

- Tag without the human's go.

## Tools

The release workflow and `just release`.

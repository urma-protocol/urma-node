# AUR package

`PKGBUILD` here builds from the `urma-node-<ver>.tar.gz` source tarball attached
to the GitHub release. Its `sha256sums` stays `SKIP` in the repository because CI
builds it from a local `git archive` tarball.

For a `v*` tag, the release job attaches `git archive` of the tag as
`urma-node-<ver>.tar.gz` and publishes `urma-node-<ver>.PKGBUILD` with `pkgver`
and that tarball's sha256 filled in, next to `urma-node.install`.
To update the AUR package:

```sh
git clone ssh://aur@aur.archlinux.org/urma-node.git && cd urma-node
curl -fLo PKGBUILD https://github.com/urma-protocol/urma-node/releases/download/v<ver>/urma-node-<ver>.PKGBUILD
curl -fLO https://github.com/urma-protocol/urma-node/releases/download/v<ver>/urma-node.install
makepkg --printsrcinfo >.SRCINFO
git add PKGBUILD urma-node.install .SRCINFO && git commit -m "urma-node <ver>" && git push
```

#!/usr/bin/env bash
# Publishes the GitHub release for a v* tag from the tested deb and NSIS artifacts.
set -euo pipefail

tag=$1
ver=${tag#v}
crate=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
if [ "$ver" != "$crate" ]; then
    echo "tag $tag does not match the workspace version $crate" >&2
    exit 1
fi

mkdir -p out
cp in/deb/*_"$ver"_amd64.deb "out/urma-node_${ver}_amd64.deb"
for script in postinst prerm postrm; do
    dpkg-deb --ctrl-tarfile "out/urma-node_${ver}_amd64.deb" | tar -t | grep -qE "^(\./)?$script\$"
done
cp in/nsis/*_"$ver"_x64-setup.exe "out/urma-node_${ver}_x64-setup.exe"

git archive --format=tar.gz --prefix="urma-node-$ver/" -o "out/urma-node-$ver.tar.gz" "$tag"
sum=$(sha256sum "out/urma-node-$ver.tar.gz" | cut -d' ' -f1)
sed -e "s/^pkgver=.*/pkgver=$ver/" -e "s/^pkgrel=.*/pkgrel=1/" \
    -e "s/^sha256sums=.*/sha256sums=('$sum')/" \
    contrib/aur/PKGBUILD >"out/urma-node-$ver.PKGBUILD"
grep -qx "sha256sums=('$sum')" "out/urma-node-$ver.PKGBUILD"
cp contrib/aur/urma-node.install out/

(cd out && sha256sum -- * >SHA256SUMS)
cat out/SHA256SUMS

gh release create "$tag" out/* --verify-tag --title "URMA Node $ver" --notes "\
URMA Node $ver: the urma-node service, urma-node-control and the desktop app.

- Debian/Ubuntu: \`sudo apt install ./urma-node_${ver}_amd64.deb\`
- Windows: run \`urma-node_${ver}_x64-setup.exe\` (per-machine, installs the urma-node service)
- Arch: \`urma-node-$ver.PKGBUILD\` (save as PKGBUILD) and \`urma-node.install\`, then \`makepkg -si\`; it builds from \`urma-node-$ver.tar.gz\` attached here

Check downloads against SHA256SUMS."

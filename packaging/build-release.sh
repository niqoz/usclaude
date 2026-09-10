#!/bin/sh
# Construit les fichiers de release de usclaude dans target/dist/ :
#   usclaude-<version>-x86_64-linux.tar.gz   binaire statique, README, LICENSE
#   usclaude_<version>_amd64.deb             paquet Debian / Ubuntu / Mint
#
# Le binaire est lié statiquement (musl) : il tourne sur toute distribution
# x86_64, quelle que soit sa version de glibc.
#
# Prérequis : cargo, la cible musl (rustup target add x86_64-unknown-linux-musl),
# musl-tools, dpkg-deb et fakeroot.
set -eu

cd "$(dirname "$0")/.."

TARGET=x86_64-unknown-linux-musl
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
DIST=target/dist
BIN="target/$TARGET/release/usclaude"

for tool in cargo musl-gcc dpkg-deb fakeroot; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "outil manquant : $tool" >&2
        exit 1
    }
done

cargo build --release --target "$TARGET"
file "$BIN" | grep -q 'static' || {
    echo "le binaire n'est pas statique : $BIN" >&2
    exit 1
}

rm -rf "$DIST"
mkdir -p "$DIST"

# ---------- Archive ----------
TAR_NAME="usclaude-${VERSION}-x86_64-linux"
install -d "$DIST/$TAR_NAME"
install -m 755 "$BIN" "$DIST/$TAR_NAME/usclaude"
install -m 644 README.md LICENSE "$DIST/$TAR_NAME/"
tar -C "$DIST" --owner=0 --group=0 -czf "$DIST/$TAR_NAME.tar.gz" "$TAR_NAME"
rm -rf "${DIST:?}/$TAR_NAME"

# ---------- Paquet Debian ----------
DEB_NAME="usclaude_${VERSION}_amd64"
ROOT="$DIST/$DEB_NAME"
install -d "$ROOT/DEBIAN" "$ROOT/usr/bin" "$ROOT/usr/share/applications" \
    "$ROOT/usr/share/doc/usclaude"

install -m 755 "$BIN" "$ROOT/usr/bin/usclaude"
install -m 644 packaging/usclaude.desktop "$ROOT/usr/share/applications/usclaude.desktop"
install -m 644 LICENSE "$ROOT/usr/share/doc/usclaude/copyright"
install -m 644 README.md "$ROOT/usr/share/doc/usclaude/README.md"
printf 'usclaude (%s) unstable; urgency=low\n\n  * Paquet construit depuis les sources.\n\n -- niqoz <niqoz@users.noreply.github.com>  %s\n' \
    "$VERSION" "$(date -R)" | gzip -9n > "$ROOT/usr/share/doc/usclaude/changelog.Debian.gz"
chmod 644 "$ROOT/usr/share/doc/usclaude/changelog.Debian.gz"

# Binaire statique : aucune dépendance, pas même libc6.
cat > "$ROOT/DEBIAN/control" <<EOF
Package: usclaude
Version: $VERSION
Section: utils
Priority: optional
Architecture: amd64
Maintainer: niqoz <niqoz@users.noreply.github.com>
Homepage: https://github.com/niqoz/usclaude
Description: limites d'usage de Claude Code dans la zone de notification
 usclaude affiche dans le panneau les limites de la commande /usage de
 Claude Code : session de 5 heures et limites hebdomadaires, avec l'heure
 de remise à zéro. Nécessite un panneau compatible StatusNotifierItem.
EOF

fakeroot dpkg-deb --build --root-owner-group "$ROOT" "$DIST/$DEB_NAME.deb" >/dev/null
rm -rf "${ROOT:?}"

ls -l "$DIST"

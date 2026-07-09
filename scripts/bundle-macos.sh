#!/usr/bin/env bash
# Bundle cuemesh2 binaries with GStreamer dylibs into a portable .tar.gz
# for macOS.
set -euo pipefail

VERSION="${1:?usage: $0 <version-tag>}"
ARCHIVE_NAME="cuemesh2-${VERSION}-macos"
STAGING="dist/${ARCHIVE_NAME}"
LIBDIR="${STAGING}/lib"
PLUGDIR="${STAGING}/plugins"
BINDIR="${STAGING}"

rm -rf "$STAGING"
mkdir -p "$LIBDIR" "$PLUGDIR"

echo "==> Copying universal binaries ..."
cp target/release/cuemesh2-controller "$BINDIR/"
cp target/release/cuemesh2-client "$BINDIR/"

# Versions/1.0 is where the .pkg actually installs; the Versions/Current
# symlink is not reliably present on CI runners.
GST_FRAMEWORK="/Library/Frameworks/GStreamer.framework/Versions/1.0"
GST_PLUGIN_DIR="${GST_FRAMEWORK}/lib/gstreamer-1.0"

# We deliberately do NOT use dylibbundler's dependency-walking here: it
# resolves the client's @rpath/libgst*.dylib references unreliably (in
# practice it produced an empty lib/ dir with no error). Instead, copy the
# framework's whole flat lib/ directory verbatim — same brute-force
# approach already used for plugins/ below — and let dyld resolve
# @rpath/@loader_path references via DYLD_LIBRARY_PATH (set in the launcher
# scripts), which overrides by leaf filename regardless of how a library
# was originally referenced. GStreamer's own lib/ is flat (only
# gstreamer-1.0/ and pkgconfig/ are subdirectories), so a shallow copy is
# exactly the runtime shared-library set, nothing more.
echo "==> Copying GStreamer runtime libraries ..."
if [ -d "$GST_FRAMEWORK/lib" ]; then
  find "$GST_FRAMEWORK/lib" -maxdepth 1 -type f -name '*.dylib' -exec cp -a {} "$LIBDIR/" \;
else
  echo "WARNING: could not find GStreamer lib directory at ${GST_FRAMEWORK}/lib. Runtime libs not bundled."
fi
echo "    $(find "$LIBDIR" -name '*.dylib' | wc -l | tr -d ' ') dylibs copied to lib/"

echo "==> Copying GStreamer plugins ..."
if [ -d "$GST_PLUGIN_DIR" ]; then
  # Plugin .dylib/.la/.a only — the macOS gl plugin ships an include/
  # subdirectory of C headers alongside its binaries that runtime users
  # don't need.
  find "$GST_PLUGIN_DIR" -maxdepth 1 -type f -exec cp -a {} "$PLUGDIR/" \;
else
  echo "WARNING: could not find GStreamer plugin directory at ${GST_PLUGIN_DIR}. Plugins not bundled."
fi

# Ad-hoc codesign so first-launch Gatekeeper behavior is at least
# consistent (this does not satisfy notarization; users still need to
# right-click → Open or clear the quarantine flag on an unsigned download).
echo "==> Ad-hoc signing binaries ..."
codesign --force --sign - "$BINDIR/cuemesh2-controller"
codesign --force --sign - "$BINDIR/cuemesh2-client"

echo "==> Creating launcher scripts ..."
cat > "$BINDIR/run-controller.sh" << 'SCRIPT'
#!/bin/bash
DIR="$(cd "$(dirname "$0")" && pwd)"
export DYLD_LIBRARY_PATH="$DIR/lib:$DIR/plugins"
export GST_PLUGIN_PATH="$DIR/plugins"
exec "$DIR/cuemesh2-controller" "$@"
SCRIPT

cat > "$BINDIR/run-client.sh" << 'SCRIPT'
#!/bin/bash
DIR="$(cd "$(dirname "$0")" && pwd)"
export DYLD_LIBRARY_PATH="$DIR/lib:$DIR/plugins"
export GST_PLUGIN_PATH="$DIR/plugins"
exec "$DIR/cuemesh2-client" "$@"
SCRIPT

chmod +x "$BINDIR/run-controller.sh" "$BINDIR/run-client.sh"

echo "==> Creating archive ..."
mkdir -p dist
tar czf "dist/${ARCHIVE_NAME}.tar.gz" -C "$(dirname "$STAGING")" "$(basename "$STAGING")"

echo "Done: dist/${ARCHIVE_NAME}.tar.gz"

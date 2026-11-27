#!/bin/sh
# Build Linen for Windows from Linux (cross-compiled with MinGW) and pack a portable
# folder + zip: Linen.exe with libmpv, Python (embeddable) and Node beside it.
#
# Needs: a Rust toolchain with the x86_64-pc-windows-gnu target, x86_64-w64-mingw32-gcc,
# 7z, and the Windows pieces in $DEPS (fetched on the first run):
#   mpv-dev (libmpv-2.dll, libmpv.dll.a)  github.com/shinchiro/mpv-winbuild-cmake
#   python-embed.zip                      python.org (Windows embeddable package)
#   node.exe                              nodejs.org
set -e
cd "$(dirname "$0")/.."
DEPS="${DEPS:-$HOME/.local/share/linen-win-deps}"
# a separate rustup install (keeps the system Rust as it is)
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.local/share/rustup-win}"
export CARGO_HOME="${CARGO_HOME:-$RUSTUP_HOME/cargo}"
CARGO="$CARGO_HOME/bin/cargo"
[ -x "$CARGO" ] || CARGO=cargo

PY_VER=3.14.10
MPV_TAG=20261111
MPV_FILE=mpv-dev-x86_64-20261111-git-bcde6b93cc.7z

mkdir -p "$DEPS"
if [ ! -f "$DEPS/mpv/libmpv-2.dll" ]; then
    curl -sSfL -o "$DEPS/mpv-dev.7z" "https://github.com/shinchiro/mpv-winbuild-cmake/releases/download/$MPV_TAG/$MPV_FILE"
    7z x -y -o"$DEPS/mpv" "$DEPS/mpv-dev.7z" >/dev/null
fi
[ -f "$DEPS/python-embed.zip" ] || curl -sSfL -o "$DEPS/python-embed.zip" "https://www.python.org/ftp/python/$PY_VER/python-$PY_VER-embed-amd64.zip"
[ -f "$DEPS/node.exe" ] || curl -sSfL -o "$DEPS/node.exe" "https://nodejs.org/dist/latest-v24.x/win-x64/node.exe"

T=x86_64-pc-windows-gnu
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar
# Some MinGW toolchains (built static-only, like Gentoo's mingw64-toolchain) keep the
# unwinder inside libgcc.a and have no libgcc_eh.a, which Rust asks for: an empty one does
if [ ! -f "$(x86_64-w64-mingw32-gcc -print-file-name=libgcc_eh.a)" ]; then
    mkdir -p "$DEPS/shim"
    [ -f "$DEPS/shim/libgcc_eh.a" ] || x86_64-w64-mingw32-ar rcs "$DEPS/shim/libgcc_eh.a"
    SHIM="-L $DEPS/shim"
fi
# libmpv's import library
export RUSTFLAGS="-L $DEPS/mpv ${SHIM:-}"
"$CARGO" build --release --target $T -p linen-player

OUT=dist/Linen
rm -rf "$OUT" && mkdir -p "$OUT/python" "$OUT/node"
cp "target/$T/release/linen.exe" "$OUT/Linen.exe"
x86_64-w64-mingw32-strip "$OUT/Linen.exe" 2>/dev/null || true
cp "$DEPS/mpv/libmpv-2.dll" "$OUT/"
7z x -y -o"$OUT/python" "$DEPS/python-embed.zip" >/dev/null
cp "$DEPS/node.exe" "$OUT/node/"
cp LICENSE "$OUT/LICENSE.txt"
cp app/ui/fonts/OFL-Manrope.txt app/ui/fonts/LICENSE-MaterialSymbols.txt "$OUT/"
cat > "$OUT/README.txt" <<'EOF'
Linen: YouTube, calmly.            https://bugzysleeps.github.io/Linen/

Double-click Linen.exe to start. Nothing to install: this folder is the whole app,
and you can put it anywhere (for example in your Documents folder).

Your library (follows, history, playlists) is kept in %APPDATA%\linen.
Downloaded videos go to your Videos\Linen folder.

Inside this folder: libmpv (plays the video), Python and Node (Linen uses them to
find a video's streams with yt-dlp, which Linen downloads and keeps up to date).

Linen is free software under the GNU GPL v3 (LICENSE.txt). Not affiliated with
YouTube or Google.
EOF
rm -f dist/Linen-windows-x64.zip
(cd dist && 7z a -tzip -mx=7 Linen-windows-x64.zip Linen >/dev/null)
echo "built: $(du -sh "$OUT" | cut -f1) folder, $(du -h dist/Linen-windows-x64.zip | cut -f1) zip"

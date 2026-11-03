#!/usr/bin/env bash
# Incremental development builds inside the Flatpak SDK.
#
# Prompts for the architecture(s) and what to produce, then writes .flatpak
# bundles to dist/ and/or installs the build for the current user. The crate
# cache and each architecture's compiled output persist in .flatpak/, so only
# changed code is recompiled. A Flatpak bundle holds a single architecture, so
# "Both" writes one file per architecture.
#
# Each Cargo profile keeps its own compiled output, so switching between them
# does not throw away the others.

set -euo pipefail

cd "$(dirname "$0")/.."

APP_ID=net.dunevent.Cajole
MANIFEST=build-aux/$APP_ID.json
MODULE=cajole
RUNTIME_REPO=https://dl.flathub.org/repo/flathub.flatpakrepo
DIST=dist

host_arch=$(flatpak --default-arch)
other_arch=$([[ $host_arch == x86_64 ]] && echo aarch64 || echo x86_64)

choose() {
    local prompt=$1 default=$2
    shift 2
    local i=1
    for option in "$@"; do
        echo "  $i) $option" >&2
        i=$((i + 1))
    done
    local reply
    read -rp "$prompt [$default]: " reply
    reply=${reply:-$default}
    if [[ ! $reply =~ ^[0-9]+$ ]] || ((reply < 1 || reply > $#)); then
        echo "Unknown choice: $reply" >&2
        exit 2
    fi
    echo "${!reply}"
}

echo "Architecture:"
# Assigned before matching, because a failed choice inside `case $(...)` would
# not stop the script.
arch_choice=$(choose "Choose" 1 "$host_arch (this machine)" "$other_arch" "Both")
case $arch_choice in
    Both) arches=("$host_arch" "$other_arch") ;;
    "$other_arch") arches=("$other_arch") ;;
    *) arches=("$host_arch") ;;
esac

# A build for another architecture cannot be installed here, so it always
# produces a file, and "Install only" is offered only for this machine alone.
if [[ ${#arches[@]} -eq 1 && ${arches[0]} != "$host_arch" ]]; then
    target="Flatpak file"
elif [[ ${#arches[@]} -eq 2 ]]; then
    echo "Target:"
    target_choice=$(choose "Choose" 1 "Flatpak files" "Flatpak files and install")
    case $target_choice in
        "Flatpak files") target="Flatpak file" ;;
        *) target="Flatpak file and install" ;;
    esac
else
    echo "Target:"
    target=$(choose "Choose" 1 "Flatpak file" "Install only" "Flatpak file and install")
fi

echo "Build profile:"
profile_choice=$(choose "Choose" 1 \
    "Fast release: optimised, quick to rebuild" \
    "Debug: quickest to build, slower to run" \
    "Release: as published, slowest to build")
case $profile_choice in
    Debug*) profile=dev profile_dir=debug ;;
    Release*) profile=release profile_dir=release ;;
    *) profile=release-dev profile_dir=release-dev ;;
esac

if command -v flatpak >/dev/null && flatpak info org.flatpak.Builder >/dev/null 2>&1; then
    builder=(flatpak run org.flatpak.Builder)
elif command -v flatpak-builder >/dev/null; then
    builder=(flatpak-builder)
else
    echo "Install org.flatpak.Builder from Flathub, or flatpak-builder." >&2
    exit 1
fi

manifest_value() {
    python3 -c '
import json, sys
manifest = json.load(open(sys.argv[1]))
if sys.argv[2] == "finish-args":
    print("--command=" + manifest["command"])
    for arg in manifest.get("finish-args", []):
        print(arg)
else:
    print(manifest[sys.argv[2]])
' "$MANIFEST" "$1"
}

sdk=$(manifest_value sdk)/ARCH/$(manifest_value runtime-version)

# Sets up the build directory with the SDK and the Rust extension. The
# manifest has no dependency modules, so nothing is compiled here.
build_deps() {
    local arch=$1 state=.flatpak/$1
    echo "==> Preparing the SDK for $arch"
    # Missing SDKs go to whichever installation already holds this
    # architecture's SDK, and system-wide when neither does. flatpak-builder
    # only looks in the installation it is told to use.
    local ref=${sdk/ARCH/$arch} scope=()
    if ! flatpak --system info "$ref" >/dev/null 2>&1 &&
        flatpak --user info "$ref" >/dev/null 2>&1; then
        scope=(--user)
    fi
    "${builder[@]}" "${scope[@]}" --arch="$arch" --force-clean --stop-at="$MODULE" \
        --state-dir=.flatpak/builder --install-deps-from=flathub \
        "$state/build" "$MANIFEST"
    sha256sum "$MANIFEST" >"$state/manifest.sha256"
}

build_app() {
    local arch=$1 state=.flatpak/$1
    mkdir -p "$state"
    # The SDK directory is rebuilt only when the manifest changes.
    if [[ ! -f $state/build/metadata ]] || ! sha256sum --status -c "$state/manifest.sha256" 2>/dev/null; then
        build_deps "$arch"
    fi
    # Crates download into a cache shared by both architectures; compiled
    # output is kept per architecture.
    local sdk_shell=(flatpak build --share=network --filesystem="$PWD"
        --env=PATH=/app/bin:/usr/bin:/usr/lib/sdk/rust-stable/bin
        --env=CARGO_HOME="$PWD/.flatpak/cargo"
        --env=CARGO_TARGET_DIR="$PWD/$state/target"
        --env=CARGO_TERM_COLOR=always "$state/build")
    # Cargo.lock is optional until one has been generated and committed.
    local locked=()
    [[ -f Cargo.lock ]] && locked=(--locked)
    echo "==> Compiling $arch ($profile)"
    "${sdk_shell[@]}" cargo build --profile "$profile" "${locked[@]}"
    # shellcheck disable=SC2016 # $1 expands inside the sandbox.
    "${sdk_shell[@]}" sh -c '
        set -e
        install -Dm755 "$1/cajole" /app/bin/cajole
        install -Dm644 data/net.dunevent.Cajole.desktop /app/share/applications/net.dunevent.Cajole.desktop
        install -Dm644 data/icons/hicolor/scalable/apps/net.dunevent.Cajole.svg /app/share/icons/hicolor/scalable/apps/net.dunevent.Cajole.svg
        install -Dm644 data/icons/hicolor/symbolic/apps/net.dunevent.Cajole-symbolic.svg /app/share/icons/hicolor/symbolic/apps/net.dunevent.Cajole-symbolic.svg
        install -Dm644 data/net.dunevent.Cajole.metainfo.xml /app/share/metainfo/net.dunevent.Cajole.metainfo.xml
        install -Dm644 data/flatpak/org.gnome.settings-daemon.plugins.power.gschema.xml /app/share/glib-2.0/schemas/org.gnome.settings-daemon.plugins.power.gschema.xml
        glib-compile-schemas /app/share/glib-2.0/schemas
    ' sh "$PWD/$state/target/$profile_dir"
}

make_bundle() {
    local arch=$1 bundle=$2 state=.flatpak/$1
    echo "==> Bundling $bundle"
    # build-finish can only run once on a directory, so a copy is finished and
    # the build directory stays usable for incremental builds.
    local finish=$state/finish
    rm -rf "$finish"
    cp -a --reflink=auto "$state/build" "$finish"
    flatpak build "$finish" strip --strip-debug /app/bin/cajole

    local finish_args
    mapfile -t finish_args < <(manifest_value finish-args)
    flatpak build-finish "${finish_args[@]}" "$finish" >/dev/null
    flatpak build-export --arch="$arch" "$state/repo" "$finish" >/dev/null
    mkdir -p "$(dirname "$bundle")"
    flatpak build-bundle --arch="$arch" --runtime-repo="$RUNTIME_REPO" \
        "$state/repo" "$bundle" "$APP_ID"
    rm -rf "$finish"
}

for arch in "${arches[@]}"; do
    if [[ $arch != "$host_arch" ]] && ! compgen -G "/proc/sys/fs/binfmt_misc/*$arch*" >/dev/null; then
        echo "Building for $arch on $host_arch needs QEMU user emulation registered" >&2
        echo "with binfmt_misc (qemu-user-static or qemu-user-binfmt)." >&2
        exit 1
    fi
done

outputs=()
for arch in "${arches[@]}"; do
    build_app "$arch"

    install=false
    file=true
    if [[ $arch == "$host_arch" ]]; then
        [[ $target == "Flatpak file" ]] || install=true
        [[ $target == "Install only" ]] && file=false
    fi

    if $file; then
        bundle=$DIST/$APP_ID-$arch.flatpak
        make_bundle "$arch" "$bundle"
        outputs+=("$bundle")
    fi
    if $install; then
        if $file; then
            source_bundle=$bundle
        else
            source_bundle=.flatpak/$arch/install.flatpak
            make_bundle "$arch" "$source_bundle"
        fi
        echo "==> Installing $arch for this user"
        flatpak install --user --reinstall -y "$source_bundle"
        $file || rm -f "$source_bundle"
    fi
done

echo
for output in "${outputs[@]}"; do
    echo "Wrote $output"
done

# Desktop integration files

`net.dunevent.Cajole.desktop` must be installed, and its name must match the
app ID exactly. GNOME Shell drops notifications from an app that has no
matching `.desktop` file, with no error at the D-Bus level. GNOME's
`GNotification` documentation and the GNOME Shell wiki both state this.

`Exec=cajole` must also resolve on `$PATH`. GIO treats a `.desktop` file whose
`Exec=` cannot be resolved as if it did not exist.

`net.dunevent.Cajole.service` is for the Flatpak only. It starts
`cajole --hidden` through bus activation, which Flatpak exports as a separate
sandbox instance. The UI uses it to start the background process: a process
the UI spawns itself dies when the UI's sandbox exits. There is no
`DBusActivatable` in the `.desktop` file. An earlier `.service` file for the
windowed app was removed after three `cajole` processes were seen lingering
with no window.

## Installing for local testing

From the project root, after `cargo build`:

```sh
mkdir -p ~/.local/bin ~/.local/share/applications
ln -sf "$(pwd)/target/debug/cajole" ~/.local/bin/cajole
cp data/net.dunevent.Cajole.desktop ~/.local/share/applications/
which cajole
```

`which cajole` should print `~/.local/bin/cajole`. To check that GIO can find
the entry:

```sh
python3 -c "
import gi
gi.require_version('Gio', '2.0')
from gi.repository import Gio
info = Gio.DesktopAppInfo.new('net.dunevent.Cajole.desktop')
print('OK:', info.get_name()) if info else print('NOT FOUND')
"
```

If it prints `NOT FOUND`, check `which cajole` first.

## Packaging

`cargo deb` (see `[package.metadata.deb]` in `Cargo.toml`) installs the
`.desktop` file, the app and symbolic icons, and the binary. The Flatpak
manifest in `build-aux/` installs the same files, plus the metainfo.

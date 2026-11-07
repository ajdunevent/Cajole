# Alarm row badge icons

Live on the main alarm list's rows — see the badge-building code in
`refresh_list_impl` (`src/main.rs`) for exactly how each condition maps
to an icon.

| Condition                  | Icon                                  |
|-----------------------------|----------------------------------------|
| Gentle Mode enabled          | `sentiment-satisfied-symbolic.svg`     |
| An alarm tone is set (not "(None)") | `music-note-outline-symbolic.svg` |
| Vibration enabled            | `phone-vibrate-symbolic.svg`           |
| Screen glow enabled          | `image-adjust-brightness-symbolic.svg` |

This copy (under `assets/`) is the original, kept as a reference/backup
— the copy actually compiled into the app lives in `resources/icons/`,
bundled via GResource (`resources/cajole.gresource.xml`, compiled by
`build.rs`), not installed under any system icon theme directory.
Deliberate choice over installing to `hicolor`: badge icons are
internal-UI-only, never seen by anything outside Cajole itself (unlike
the real app icon, which does need a real hicolor install to be found
by the shell/app-grid/notifications), and bundling keeps `hicolor`
itself free of anything beyond that one real app-identity icon.

## Fill color — tested, no change needed

These use `fill="#222222"`. Earlier guidance here said this needed
changing to `#2e3436` before these would recolor correctly — that was
wrong, and worth correcting rather than leaving as stale advice.

Tested directly, twice — once via real theme-directory installation
(`Adwaita`'s own `symbolic/actions/`), and again via the actual
GResource path this project ended up using — loading each through GTK's
real icon-name lookup (`Gtk.Image.new_from_icon_name`, never a direct
file load) next to known-good system symbolic icons, comparing forced
light and forced dark color schemes. Recolored identically to the
system icons both times — correctly inverting to white in dark mode,
not staying fixed at the original fill. GTK's symbolic recoloring works
by using the icon's shape as an alpha mask once it's loaded through a
proper name-based lookup path — it isn't keyed to matching a specific
source fill color. `#2e3436` is a common convention among icon
designers, not a functional requirement.

One real finding from chasing the theme-directory version of this test
down: `hicolor` (the bare fallback theme) doesn't declare a
`symbolic/actions` directory on this system — only `symbolic/apps`,
which is why Cajole's own app icon works there. The actual active
desktop theme (`Adwaita`) does declare `symbolic/actions`,
`symbolic/status`, and others. Not relevant to how these actually
shipped in the end (GResource, not a `hicolor` install), but worth
remembering if that ever changes.

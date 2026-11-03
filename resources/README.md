# Resources

`cajole.gresource.xml` lists the files compiled into the app by `build.rs`.

## Badge icons

The `icons/` files are the symbolic icons on the alarm list's rows. See
`refresh_list_impl` in `src/main.rs` for how each condition maps to an icon.

| Condition | Icon |
| --- | --- |
| Gentle Mode enabled | `sentiment-satisfied-symbolic.svg` |
| An alarm tone is set (not "(None)") | `music-note-outline-symbolic.svg` |
| Vibration enabled | `phone-vibrate-symbolic.svg` |
| Screen glow enabled | `image-adjust-brightness-symbolic.svg` |

They are bundled through GResource instead of installed under `hicolor`,
since only Cajole uses them. The app icon is the only icon installed under
`hicolor`, because the shell, app grid and notifications must find it.

The files use `fill="#222222"`. GTK recolors a symbolic icon loaded by icon
name (`Gtk.Image.new_from_icon_name`) from its shape, so the fill value does
not matter, and `#2e3436` is only a convention. Tested in forced light and dark
color schemes, both through a theme directory and through GResource.

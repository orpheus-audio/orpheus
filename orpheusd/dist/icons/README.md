# Icons

Device icons that the common icon themes don't install. They come from GNOME's
[icon development kit](https://gitlab.gnome.org/Teams/Design/icon-development-kit)
and are licensed CC0-1.0:

| Orpheus name | Original | Used for |
|---|---|---|
| `orpheus-network-speaker` | `wireless-speaker` | outputs of unknown type (default) |
| `orpheus-speaker` | `speaker-front` | speakers |
| `orpheus-hifi` | `sound` | amplifiers, AV receivers |

The `orpheus-` prefix keeps icon themes from replacing them with unrelated
icons of the same name.

They go into the `hicolor` theme, which every icon theme falls back to:

```sh
cp -P hicolor/scalable/devices/* /usr/share/icons/hicolor/scalable/devices/
gtk-update-icon-cache /usr/share/icons/hicolor
```

For a single user:

```sh
mkdir -p ~/.local/share/icons/hicolor/scalable/devices
cp -P hicolor/scalable/devices/* ~/.local/share/icons/hicolor/scalable/devices/
# Only needed if the directory already has an icon-theme.cache, which would
# hide the new files; -t because there is no index.theme there.
gtk-update-icon-cache -f -t ~/.local/share/icons/hicolor
```

Each icon exists as `name-symbolic.svg` and as `name.svg` (a link to it), because
some desktops ask for the symbolic name and others for the plain one.

# App icon

Eihwaz (ᛇ), the yew rune: a dark graphite stroke on a light silver tile, in the macOS icon grid
(824 px body in a 1024 px canvas, corners about 22.5% of the width, soft drop shadow).

Regenerate:

    swift design/make_icon.swift src-tauri/app-icon.png x silver

Other palettes the script knows: evergreen, graphite, indigo, heartwood.
    npx tauri icon src-tauri/app-icon.png    # then delete icons/android and icons/ios

The in-app mark (`.logo` in src/style.css) is the same design as an inline SVG.

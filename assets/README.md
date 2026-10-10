# Application icon

The teal tile and silver disk layers represent an archived base image and its
writable overlays. The artwork was generated with the built-in imagegen tool,
then exported to standard application sizes without changing the design.

- `icon-source.png`: original high-resolution artwork with transparency.
- `icon.png`: 512 × 512 RGBA image embedded in the window and application header.
- `icon.ico`: 16, 20, 24, 32, 40, 48, 64, 128 and 256 px Windows resources.

To export the PNG and ICO again with Python and Pillow installed:

```powershell
python scripts/export-icon.py
```

## Generation prompt

Create one production-ready Windows desktop application icon for VhdxDock, a tool for virtual disk archives and writable differencing layers. A single premium polished icon centered on a transparent square canvas, no contact sheet, no surrounding mockup, no text, no letters, no numbers, no watermark. Shape: a substantial rounded square / softly rounded squircle tile in deep petroleum teal #0d7377 with a restrained brighter turquoise upper-left gradient, transparent exterior corners. Inside the tile, a bold clean silver-white symbol of three stacked virtual disk trays, the uppermost tray hovering slightly above the lower two and connecting visually into a compact dock. Use a coherent rounded isometric perspective with broad filled surfaces and clear negative gaps between layers, subtle realistic bevel highlights and very restrained depth/shadows, elegant Microsoft Fluent-inspired craftsmanship, minimal geometry, no thin wireframes, no blue cloud, no detailed circuit boards, no glass clutter. Center the white stack with generous even breathing room. Strong distinctive silhouette, crisp edges, excellent readability as a 32px/24px taskbar icon. The rounded teal tile should occupy about 88 percent of the square canvas, fully visible and uncropped. Favor restrained light silver and warm white on rich teal, professional storage utility aesthetic. Render at high resolution, square composition, genuinely transparent outside the rounded tile, no checkerboard drawn into the picture.

## Edge cleanup prompt

Edit the provided VhdxDock icon. Preserve the rounded teal tile and the three silver-white stacked disk trays, their geometry, arrangement, materials, teal/silver palette, lighting and perspective exactly. Only clean the exterior alpha silhouette: remove every isolated stray teal/blue pixel, speck, particle, fringe, scratch or floating fragment outside the rounded square tile, especially the specks above the tile and along the left edge. Make the entire canvas outside the continuous rounded tile truly alpha-transparent. Give the exterior a smooth clean antialiased contour, no jagged colored fragments. Keep even transparent padding on every side and a centered square composition. Do not add or change text, objects, backgrounds or the icon design. No checkerboard pattern.

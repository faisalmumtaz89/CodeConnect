# Branding

Master artwork for the app icon. `AppIcon.svg` is the source of truth; the PNGs in
`CodeConnect/Assets.xcassets/AppIcon.appiconset/` are generated from it and should
never be edited by hand.

## Regenerating

```sh
cd ios/Branding
rsvg-convert -w 1024 -h 1024 -b '#000000' AppIcon.svg      -o ../CodeConnect/Assets.xcassets/AppIcon.appiconset/AppIcon.png
rsvg-convert -w 1024 -h 1024                AppIcon-Dark.svg -o ../CodeConnect/Assets.xcassets/AppIcon.appiconset/AppIcon-Dark.png
python3 -c "from PIL import Image; \
Image.open('../CodeConnect/Assets.xcassets/AppIcon.appiconset/AppIcon.png') \
     .convert('L').convert('RGB') \
     .save('../CodeConnect/Assets.xcassets/AppIcon.appiconset/AppIcon-Tinted.png')"
```

## The three variants, and why each is the way it is

Apple's rules differ per variant, and getting them wrong fails App Store validation
with error 90717 rather than looking slightly off:

| Variant | Background | Alpha | Colour |
|---|---|---|---|
| Default | Opaque, full-bleed | **Must not have one** | Full colour |
| Dark | **Transparent** | Required | Full colour |
| Tinted | Opaque, full-bleed | **Must not have one** | **Greyscale** |

The dark icon is transparent because iOS composites it over a backdrop it supplies
itself. The tinted icon is greyscale because the system colourises it with the
user's accent colour — supplying colour there would fight that.

None of the artwork has rounded corners: iOS applies its own mask, and baking in a
radius produces a visible double-corner.

## Why an asset catalogue rather than Icon Composer

Xcode 26 offers Icon Composer and `.icon` bundles for the Liquid Glass treatment.
An `.icon` file **replaces** the asset catalogue and takes over icon rendering on
every OS version the app runs on — including iOS 17 and 18, which this app still
supports. A single 1024×1024 asset-catalogue icon remains fully supported, and the
system applies the modern treatment to it automatically.

Revisit if the deployment target moves to iOS 26.

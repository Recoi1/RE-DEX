# Mods

The start screen's **Mods** button lists the Pokémon X and Y mods on GameBanana (most liked first, with a search) and
installs them with one press. Nothing is hosted by RE:DEX: the list and the downloads come straight from GameBanana's
public API, on your PC, when you open the screen or press Install. Each mod shows what the RE:DEX developers tested:
**Verified**, **Partial**, **Broken** or **Untested**. The test results are [`data/mod_compat.json`](../data/mod_compat.json)
in this repository, read from GitHub when the screen opens, so they can change between releases.

## What can be installed

| Kind | What RE:DEX does with it |
|---|---|
| **Texture packs** (Citra and Azahar's `tex1_<w>x<h>_<hash>_<format>.png` pictures, with or without a `pack.json`) | Drawn over the game's own textures. They need the Vulkan renderer. |
| **Game-file mods** in Citra's layout (`romfs/...`, `<title id>/romfs/...`, `romfs_ext/` patches, or loose `a/0/8/8`-style paths) | Laid over the game's files when it starts, as Citra and Luma3DS do. |

Mods that change the game's **code** (`code.bin`, `code.ips`, `code.bps`, `exheader.bin`, an `exefs` folder) can't be
installed: RE:DEX runs your game as native code built on your PC, so a patch to the 3DS code doesn't reach it. The
screen says so and offers **Open page**. Save files and loose GARC entries (`3783.bin`) aren't installable either.

Installed mods stay in `<data>/mods/installed/<GameBanana id>/` (`<data>` is `%APPDATA%\RE-DEX` unless you set
`XR_DATA`). **Turn off** keeps a mod installed but leaves it out; **Remove** deletes it. Changes show the next time you
press Play. Mods you put in the game's own mods folder by hand (`<data>/mods/<title id>/`, Citra's layout) still work.

## Several mods at once

Mods are loaded in the order you installed them. When two mods that are on change the same file, the later one wins,
and the screen says which mods clash, so you can turn one off.

## Online

Mods only change your own screen: other players never see your skin. A world lets you in while every file your mods
change is a model, texture or animation of a Pokémon or a character, or a picture of the menus (the list is in
`crates/xr-hle/src/cosmetic.rs`). Texture packs always count. A mod that changes anything else (stats, moves,
encounters, trainers, items, text, scripts, maps, code) is marked **Offline only**, and joining a world says
"Turn off mods to play online: <mod> changes the game". Turn it off on the Mods screen to play online again.

## Safety

An archive from GameBanana is checked before it is unpacked (with your system's `tar`): files that would land outside
its folder, links, device names, and archives over 2 GB are refused. Nothing in an archive is ever run. Downloads only
come from GameBanana's own servers, over https, and the requests carry no personal data (`User-Agent: RE:DEX/<version>`).

## Adding a test result

After testing a mod with RE:DEX, add it to `data/mod_compat.json` with a pull request: its GameBanana id, a `status`
(`verified`, `partial` or `broken`) and a `note` saying what was tested and how it went.

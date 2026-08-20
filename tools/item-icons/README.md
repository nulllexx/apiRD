# Item icons

Renders proper inventory icons — the isometric 3D ones the game draws, not flat
sprites — for every item on the server, using
[Renderchest](https://github.com/aternosorg/renderchest).

## How it fits

The admin panel serves any PNG it finds in its texture cache **before** doing
anything else. So this tool needs no cooperation from the running API: it drops
files into that cache and they are what gets served, with no restart.

Everything the API does at runtime stays in place as the fallback for whatever
this does not render:

```
request  ->  cache hit?              <- this tool fills the cache
         ->  item model file?        <- authoritative, reads models/item/*.json
         ->  mod jar search
         ->  vanilla mirror
         ->  initials tile
```

## Running it

Build once:

```sh
docker build -t item-icons tools/item-icons
```

Then, **the first time**, inspect rather than install. Renderchest's output
layout is not documented, and a run that guesses wrong appears to succeed while
installing nothing usable:

```sh
docker run --rm \
  -v /home/useradmin/mcserver/mods:/mods:ro \
  -v /home/useradmin/api/mainapi/data/item-textures:/cache \
  item-icons inspect minecraft
```

That renders one namespace and prints the resulting file tree. If the PNG
basenames read like item ids (`diamond_sword.png`), the layout is what the
installer assumes and you can run the real thing:

```sh
docker run --rm \
  -v /home/useradmin/mcserver/mods:/mods:ro \
  -v /home/useradmin/api/mainapi/data/item-textures:/cache \
  item-icons all
```

If the basenames are something else — a sprite sheet, hashed names — stop and
say so; the installer's flattening step needs adjusting rather than forcing.

## What it does

1. Downloads the client jar for `MC_VERSION` from **Mojang's own manifest** (the
   server has no client assets, and the base game's models are required) and
   extracts its `assets/` tree.
2. Extracts `assets/` from every jar in `/mods`, **including jars bundled inside
   `META-INF/jarjar/`** — Aeronautics ships three mods that way, and skipping
   the nested jars loses all three namespaces.
3. Runs Renderchest once per namespace, vanilla assets first so modded models
   can inherit from vanilla parents.
4. Copies the PNGs into `<cache>/<version>/<namespace>/<item>.png`.

## Settings

| Variable    | Default   | Meaning                                       |
|-------------|-----------|-----------------------------------------------|
| `MC_VERSION`| `1.21.1`  | Must match the server, and the API's `MINECRAFT_ASSETS_VERSION` |
| `ICON_SIZE` | `64`      | Rendered icon size in pixels                  |
| `MODS_DIR`  | `/mods`   | Mount the server's mods directory here        |
| `CACHE_DIR` | `/cache`  | Mount the API's `ITEM_TEXTURE_DIR` here       |

`MC_VERSION` is part of the cache path, so it has to match the API's
`MINECRAFT_ASSETS_VERSION` or the icons land where nothing will look for them.

## Automatic re-runs

The deployed setup runs this as a compose sidecar in `watch` mode, so nothing
has to be triggered by hand:

```yaml
item-icons:
  image: ghcr.io/<owner>/apird-item-icons:latest
  command: ["watch"]
  volumes:
    - /home/useradmin/mcserver/mods:/mods:ro
    - /home/useradmin/api/mainapi/data/item-textures:/cache
    - item-icons-work:/work
```

It fingerprints the mods directory — each jar's name, size and modification
time — every `POLL_INTERVAL` seconds (default 300) and re-renders when that
changes.

Three details that matter:

- **It waits for the modpack to stop changing** before rendering. Copying a pack
  in takes a while, and rendering halfway through would produce icons for a
  state that never actually ran, then record that state as done.
- **The fingerprint is written only after a successful run**, so a failure is
  retried on the next tick rather than being recorded as complete.
- **Polling, not inotify.** This watches a bind mount, where inotify is
  unreliable across filesystems and silently misses events. A directory listing
  every few minutes costs nothing and cannot miss a change, only notice it a
  little late.

It is a sidecar rather than something the API triggers on purpose: triggering it
would mean giving the API a way to start containers, which is exactly the Docker
socket access the `mc-control` split exists to avoid. The two never talk — the
renderer writes PNGs into the cache, and the API serves whatever it finds there.

To force a re-render without changing the pack, delete the fingerprint:

```sh
rm /home/useradmin/api/mainapi/data/item-textures/.modpack-fingerprint
```

## When to re-run

The watcher covers modpack changes on its own. A **Minecraft version upgrade**
still needs a hand: bump `MC_VERSION` here and `MINECRAFT_ASSETS_VERSION` on the
API together, since the version is part of the cache path and a mismatch files
the icons where nothing will look for them.

Nothing expires on its own: a rendered icon is a cache hit forever, which is the
point. Removing a mod leaves its icons behind as orphans — harmless, since
nothing will ask for items that no longer exist — so a namespace directory only
needs deleting if you want the space back.

To drop a namespace's icons and let the API fall back to its own lookup again,
delete `<cache>/<version>/<namespace>/`.

## Known gaps

Renderchest carries its own definitions for the items the model system cannot
draw, in `builtin/minecraft/items/`. Taken from the v5.1.0 file listing rather
than from its README, which understates this:

| Item              | Covered |
|-------------------|---------|
| chests (all)      | yes, 65 definitions including the copper variants |
| shulker boxes     | yes, all colours |
| banners           | yes, all colours |
| mob heads         | yes, 208 definitions |
| shield            | yes |
| decorated pot     | yes |
| conduit           | yes |
| **beds**          | **no** |

So beds are the one thing expected to stay as a flat texture. Anything missing
falls back to the panel's own lookup and then to the initials tile, so a gap is
cosmetic rather than broken.

## If nothing renders on 1.21.1

Renderchest 5.x targets recent Minecraft versions — its builtin definitions use
the `assets/<ns>/items/` layout that only exists from 1.21.4, and its constants
mention trim and armour materials newer than 1.21.1. Whether it copes with an
older server's assets is untested here.

If `inspect` renders nothing, that is the likely reason, and the fix is to pin
an older major rather than to change anything else:

```sh
docker build --build-arg RENDERCHEST_VERSION='^4.0' -t item-icons tools/item-icons
```

Published majors run from 1.x to 5.x, so there is room to walk back.

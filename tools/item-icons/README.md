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

**On the server**, do not build — there is no repo checkout there, only
`docker-compose.yml`. CI publishes the image, so pull it and use that name:

```sh
IMG=ghcr.io/<owner>/apird-item-icons:latest
docker pull "$IMG"
```

Substitute `$IMG` for `item-icons` in the commands below. Building locally is
for a machine with the repo checked out:

```sh
docker build -t item-icons tools/item-icons
```

Then check it against the real assets before letting the watcher loose. This
takes a couple of minutes and answers the only question that matters — whether
the Renderchest version can read this Minecraft version's assets at all:

```sh
docker run --rm \
  -v /home/useradmin/mcserver/mods:/mods:ro \
  -v /home/useradmin/api/mainapi/data/item-textures:/cache \
  item-icons inspect minecraft
```

It renders one namespace and prints a verdict. `OK` plus a handful of sample
names means the pipeline works end to end; `FAIL` means a version mismatch and
says which pin to use. It exits non-zero on failure, so it is safe to chain.

Once it reports OK:

```sh
docker run --rm \
  -v /home/useradmin/mcserver/mods:/mods:ro \
  -v /home/useradmin/api/mainapi/data/item-textures:/cache \
  item-icons all
```

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

Renderchest writes `<output>/items/<namespace>/<item>.png`, so step 4 reads that
exact directory rather than searching the output for PNGs. The distinction
matters: Renderchest always emits `renderchest:unknown` and `renderchest:empty`
placeholders under `items/renderchest/`, **including on a run that found no real
items**, so a broad search would install those two and make a failed render look
like a small success.

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

## The Renderchest version must match the Minecraft version

This is the one setting that will silently produce nothing if it is wrong, so
it is worth understanding rather than copying.

Renderchest discovers items by scanning a directory, and the game moved that
directory in 1.21.4:

| Renderchest | Scans                        | Minecraft        |
|-------------|------------------------------|------------------|
| 2.x         | `assets/<ns>/models/item/`   | up to 1.21.3     |
| 3.x – 5.x   | `assets/<ns>/items/`         | 1.21.4 and later |

Point 3.x+ at 1.21.1 assets and that directory does not exist, so it finds zero
items, renders only its two internal placeholders, writes a stylesheet, and
**exits 0**. Nothing errors; nothing appears. This is why `render_all` now fails
explicitly on a zero-icon run instead of trusting the exit code.

The server runs 1.21.1, so the Dockerfile pins `^2.4`. On a Minecraft upgrade
past 1.21.3, this has to move to `^5.1` at the same time as `MC_VERSION`:

```sh
docker build --build-arg RENDERCHEST_VERSION='^5.1' -t item-icons tools/item-icons
```

## Known gaps

Renderchest carries its own definitions for items whose models the game draws as
entities rather than from a model file. Taken from the **v2.4.1** file listing
(160 definitions), which is the pinned version:

| Item                          | Covered |
|-------------------------------|---------|
| beds                          | yes, all 16 colours |
| banners                       | yes, all 16 colours |
| shulker boxes                 | yes, all colours |
| chest, ender chest, trapped chest | yes |
| mob heads and skulls          | yes, all 7 |
| shield                        | yes |
| decorated pot and sherds      | yes |
| conduit                       | yes |
| potions and tipped arrows     | yes |
| leather armour trims          | yes |
| **trident**                   | **no** |
| **spyglass**                  | **no** |

Note that 2.x covers **beds**, which 5.x does not — so pinning for the layout
also happens to fix the bed.

v2.4.1 predates 1.21.1 by about eight months, so anything added since that needs
special-casing will be missing here too. Ordinary items are unaffected: those
render from the server's own model files, not from these definitions.

Anything missing falls back to the panel's own lookup and then to the initials
tile, so a gap is cosmetic rather than broken.

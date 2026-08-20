#!/usr/bin/env bash
#
# Render inventory icons for every item the server can have, and lay them into
# the admin panel's texture cache.
#
# The panel serves any PNG it finds in that cache before consulting anything
# else, so this needs no cooperation from the running API: drop files in and
# they are what gets served. Everything the API does at runtime -- reading model
# files, searching mod jars, fetching the vanilla mirror -- stays in place as
# the fallback for whatever this does not render.
#
#   render-icons inspect   # render ONE namespace and show what came out
#   render-icons all       # render everything and install it into the cache
#   render-icons watch     # re-render whenever the modpack changes
#
# `inspect` exists because Renderchest's output layout is not documented, and
# guessing it would mean a run that appears to succeed while installing nothing
# usable. Run it once, confirm the file names look like item ids, then run
# `all`.

set -euo pipefail

# Exported, not merely assigned.
#
# `MC_VERSION="${MC_VERSION:-1.21.1}"` on its own creates a shell variable that
# child processes cannot see, and the manifest lookup below is a php subprocess
# reading getenv(). Under compose the variable arrives already exported from the
# `environment:` block and everything works; fall through to the default -- as a
# bare `docker run` does -- and php sees nothing, matches no version, and the
# script blames Mojang for not shipping 1.21.1.
export MODS_DIR="${MODS_DIR:-/mods}"
export CACHE_DIR="${CACHE_DIR:-/cache}"
export MC_VERSION="${MC_VERSION:-1.21.1}"
export ICON_SIZE="${ICON_SIZE:-64}"
export WORK="${WORK:-/work}"
export POLL_INTERVAL="${POLL_INTERVAL:-300}"

VANILLA_ASSETS="$WORK/vanilla/assets"
MOD_ASSETS="$WORK/mods/assets"
RENDER_OUT="$WORK/out"
# The CLI is a plain script inside the installed package, not a composer bin.
#
# Two reasons it is run as `php <script>` from RENDERCHEST_HOME rather than
# executed directly: its shebang is `#!/usr/bin/php`, while the official PHP
# image puts php at /usr/local/bin/php; and it does
# `require_once "vendor/autoload.php"` with a relative path, so it finds its
# own dependencies only when the working directory is the project root.
RENDERCHEST_HOME="${RENDERCHEST_HOME:-/opt/renderchest}"
RENDERCHEST="$RENDERCHEST_HOME/vendor/aternos/renderchest/renderchest"

log() { printf '\n== %s\n' "$*"; }

# --------------------------------------------------------------- vanilla assets
#
# Renderchest needs the base game's models and textures. They live in the client
# jar, which the server does not have, so it is fetched from Mojang's own
# manifest -- the authoritative source, and no third-party mirror involved.
fetch_vanilla() {
  if [ -d "$VANILLA_ASSETS/minecraft/models" ]; then
    log "vanilla assets already present"
    return
  fi

  log "fetching the $MC_VERSION client jar from Mojang"
  mkdir -p "$WORK/vanilla"

  local manifest version_url client_url
  manifest="https://launchermeta.mojang.com/mc/game/version_manifest_v2.json"

  # The version id is passed as an argument rather than read from the
  # environment, so this cannot break again the way it did before: an argument
  # is visible to the subprocess no matter how the caller set the variable.
  version_url="$(curl -fsSL "$manifest" \
    | php -r '$j=json_decode(stream_get_contents(STDIN),true);
              $want=$argv[1] ?? "";
              foreach($j["versions"] as $v){ if($v["id"]===$want){ echo $v["url"]; exit; } }' \
          -- "$MC_VERSION")"

  if [ -z "$version_url" ]; then
    echo "!! Could not find Minecraft '$MC_VERSION' in Mojang's version manifest." >&2
    echo "!! Manifest lists $(curl -fsSL "$manifest" | grep -o '"id"' | wc -l) versions;" >&2
    echo "!! the newest few are:" >&2
    curl -fsSL "$manifest" \
      | php -r '$j=json_decode(stream_get_contents(STDIN),true);
                foreach(array_slice($j["versions"],0,5) as $v){ echo "!!   ".$v["id"]."\n"; }' >&2
    echo "!! Check MC_VERSION -- it must be an exact id, e.g. 1.21.1" >&2
    exit 1
  fi

  client_url="$(curl -fsSL "$version_url" \
    | php -r '$j=json_decode(stream_get_contents(STDIN),true); echo $j["downloads"]["client"]["url"];')"

  curl -fsSL "$client_url" -o "$WORK/client.jar"
  # Only the asset tree matters; the class files are of no interest here.
  unzip -q -o "$WORK/client.jar" 'assets/*' -d "$WORK/vanilla"
  rm -f "$WORK/client.jar"

  log "vanilla namespaces: $(ls "$VANILLA_ASSETS" | tr '\n' ' ')"
}

# ------------------------------------------------------------------ mod assets
#
# Every jar contributes its `assets/` tree. NeoForge's JarJar bundles carry
# whole mods inside META-INF/jarjar/, and those are where a bundled mod's assets
# actually live -- Aeronautics ships three mods that way, and skipping the
# nested jars loses all three namespaces.
extract_mods() {
  log "extracting mod assets from $MODS_DIR"
  rm -rf "$WORK/mods"
  mkdir -p "$MOD_ASSETS"

  local jar nested_dir nested count=0 bundled=0
  for jar in "$MODS_DIR"/*.jar; do
    [ -e "$jar" ] || continue
    count=$((count + 1))

    unzip -q -o "$jar" 'assets/*' -d "$WORK/mods" 2>/dev/null || true

    # Unwrap bundled jars, then take their assets too.
    if unzip -l "$jar" 2>/dev/null | grep -q 'META-INF/jarjar/.*\.jar'; then
      nested_dir="$WORK/nested/$(basename "$jar" .jar)"
      mkdir -p "$nested_dir"
      unzip -q -o "$jar" 'META-INF/jarjar/*.jar' -d "$nested_dir" 2>/dev/null || true

      for nested in "$nested_dir"/META-INF/jarjar/*.jar; do
        [ -e "$nested" ] || continue
        bundled=$((bundled + 1))
        unzip -q -o "$nested" 'assets/*' -d "$WORK/mods" 2>/dev/null || true
      done
    fi
  done

  rm -rf "$WORK/nested"
  log "read $count jar(s), $bundled bundled jar(s)"
  log "mod namespaces: $(ls "$MOD_ASSETS" 2>/dev/null | tr '\n' ' ')"
}

namespaces() {
  { ls "$MOD_ASSETS" 2>/dev/null || true; echo minecraft; } | sort -u
}

render_namespace() {
  # Split across two statements on purpose: under `set -u`, bash's `local a=$1
  # b=$a` declares every name as an unset local *before* assigning, so the $a
  # on the right reads the unset local and kills the shell.
  local ns="$1"
  local out="$RENDER_OUT/$ns"
  mkdir -p "$out"

  # Vanilla assets come first: Renderchest requires the base game's assets to be
  # present regardless of which namespace is being rendered, because modded
  # models routinely inherit from vanilla parents.
  ( cd "$RENDERCHEST_HOME" && php "$RENDERCHEST" \
      --assets "$VANILLA_ASSETS" \
      --assets "$MOD_ASSETS" \
      --namespace "$ns" \
      --output "$out" \
      --format png \
      --size "$ICON_SIZE" \
  ) || echo "!! renderchest failed for namespace $ns (continuing)" >&2
}

# Copy the rendered icons into the cache.
#
# Renderchest writes <output>/items/<namespace>/<item>.<format>, so the source
# directory is read explicitly rather than by searching for PNGs anywhere under
# the output. The difference matters: a blind search also picks up the
# `renderchest:unknown` and `renderchest:empty` placeholders, which land in
# items/renderchest/ and are always produced -- including when zero real items
# were found. Installing those as `unknown.png` would turn "rendered nothing"
# into a plausible-looking success.
#
# Returns the number installed via INSTALLED, and non-zero if that is zero.
INSTALLED=0
install_namespace() {
  # Separate statements: see the note in render_namespace -- a single `local`
  # cannot reference a name it is itself declaring while `set -u` is on.
  local ns="$1"
  local dest="$CACHE_DIR/$MC_VERSION/$ns"
  local src="$RENDER_OUT/$ns/items/$ns"
  local moved=0 file

  if [ ! -d "$src" ]; then
    echo "  $ns: nothing rendered (no $src)" >&2
    INSTALLED=0
    return 1
  fi

  mkdir -p "$dest"

  # Animated textures arrive as one file per frame.
  #
  # Prismarine, sculk and friends have animated textures, and PNG cannot hold
  # more than one frame -- so ImageMagick writes prismarine_stairs-0.png through
  # prismarine_stairs-95.png and never a bare prismarine_stairs.png. (Renderchest
  # defaults to webp, which *can* animate; forcing png is what splits them.)
  # Copying those through verbatim would leave the panel asking for
  # `prismarine_stairs` and still getting a 404, so frame 0 becomes the icon and
  # the rest are dropped. An inventory slot wants a still image anyway.
  local base stem frame target
  for file in "$src"/*.png; do
    [ -e "$file" ] || continue
    base="$(basename "$file" .png)"
    target="$base.png"

    case "$base" in
      *-[0-9] | *-[0-9][0-9] | *-[0-9][0-9][0-9])
        stem="${base%-*}"
        frame="${base##*-}"
        # Only ever treat this as a frame if the whole family is frames. A real
        # item legitimately ending in -<digits> would have no siblings, and is
        # left alone rather than silently renamed.
        if [ -e "$src/$stem-0.png" ]; then
          [ "$frame" = "0" ] || continue
          target="$stem.png"
        fi
        ;;
    esac

    cp -f "$file" "$dest/$target"
    moved=$((moved + 1))
  done

  INSTALLED="$moved"
  echo "  $ns: installed $moved icon(s) into $dest"
  [ "$moved" -gt 0 ]
}

# ---------------------------------------------------------------- change watch
#
# A fingerprint of the mods directory: every jar's name, size and modification
# time. Cheap to take, and it changes for the things that matter — a jar added,
# removed, or replaced by a new version.
#
# Polling rather than inotify on purpose. This watches a bind mount, where
# inotify is unreliable across filesystems and silently misses events; a
# directory listing every few minutes costs nothing and cannot miss a change,
# only notice it slightly late.
fingerprint() {
  # `|| true` matters: with pipefail set, a missing or unreadable mods
  # directory would fail the pipeline, fail the command substitution, and
  # kill the watcher outright. An unreadable directory should read as
  # "nothing there" and be retried next tick, not end the process.
  { find "$MODS_DIR" -maxdepth 1 -name '*.jar' -printf '%f|%s|%T@\n' 2>/dev/null || true; } \
    | sort \
    | sha256sum \
    | cut -d' ' -f1
}

FINGERPRINT_FILE="$CACHE_DIR/.modpack-fingerprint"

render_all() {
  fetch_vanilla
  extract_mods
  log "rendering"
  local ns total=0
  for ns in $(namespaces); do
    echo "  rendering $ns"
    render_namespace "$ns"
  done
  log "installing into $CACHE_DIR/$MC_VERSION"
  for ns in $(namespaces); do
    # A namespace that renders nothing is not fatal on its own: a mod can
    # legitimately ship no item models at all. The total is what decides.
    install_namespace "$ns" || true
    total=$((total + INSTALLED))
  done

  # Fail when the whole run produced nothing.
  #
  # Renderchest exits 0 after finding zero items, so without this check a
  # version or layout mismatch reads as a clean run -- and in watch mode the
  # fingerprint gets written, marking a broken render as done and never
  # retrying. Silence here previously looked identical to success.
  if [ "$total" -eq 0 ]; then
    echo "!! rendered 0 icons in total." >&2
    echo "!! Almost always a version mismatch: Renderchest 3.x+ looks for items" >&2
    echo "!! in assets/<ns>/items/ (Minecraft 1.21.4+), while MC_VERSION=$MC_VERSION" >&2
    echo "!! keeps them in assets/<ns>/models/item/ and needs Renderchest 2.x." >&2
    echo "!! Rebuild with: --build-arg RENDERCHEST_VERSION='^2.4'" >&2
    return 1
  fi

  log "installed $total icon(s) in total"
}

case "${1:-}" in
  inspect)
    fetch_vanilla
    extract_mods
    ns="${2:-minecraft}"
    log "rendering only '$ns' so its output can be examined"
    render_namespace "$ns"

    log "output tree for '$ns' (first 20 entries)"
    # `|| true`: head exits after 20 lines, find takes SIGPIPE, and with
    # pipefail that 141 would kill the script before it printed its verdict.
    find "$RENDER_OUT/$ns" -maxdepth 3 | head -20 || true

    # Renderchest always emits its two placeholders under items/renderchest/,
    # even when it found no real items, so the count that matters is the one in
    # the requested namespace's own directory.
    rendered=$(find "$RENDER_OUT/$ns/items/$ns" -maxdepth 1 -name '*.png' 2>/dev/null | wc -l)
    log "rendered $rendered icon(s) for '$ns'"

    if [ "$rendered" -eq 0 ]; then
      echo "FAIL: Renderchest found no items in namespace '$ns'."
      echo
      echo "For MC_VERSION=$MC_VERSION the item models are in"
      echo "  assets/$ns/models/item/     -> needs Renderchest 2.x"
      echo "whereas Renderchest 3.x and later look in"
      echo "  assets/$ns/items/           -> Minecraft 1.21.4 and later"
      echo
      echo "Rebuild with: --build-arg RENDERCHEST_VERSION='^2.4'"
      exit 1
    fi

    echo "OK: sample icons ->"
    find "$RENDER_OUT/$ns/items/$ns" -maxdepth 1 -name '*.png' | head -5 | sed 's|.*/|  |' || true
    echo
    echo "Layout is as expected. Run: render-icons all"
    ;;

  all)
    # The fingerprint is written only on success. Recording a failed render as
    # done is what turns a one-off problem into a permanent one, because the
    # watcher then sees no change and never tries again.
    if ! render_all; then
      echo "!! rendering failed; fingerprint not written, nothing installed" >&2
      exit 1
    fi
    fingerprint > "$FINGERPRINT_FILE"
    log "done — reload the admin panel, no API restart needed"
    ;;

  watch)
    log "watching $MODS_DIR every ${POLL_INTERVAL}s"
    previous=""
    [ -f "$FINGERPRINT_FILE" ] && previous="$(cat "$FINGERPRINT_FILE")"
    settling=""

    while :; do
      current="$(fingerprint)"

      if [ "$current" = "$previous" ]; then
        settling=""
        sleep "$POLL_INTERVAL"
        continue
      fi

      # Wait for the directory to stop moving before rendering. Copying a
      # modpack in takes a while, and rendering halfway through it would
      # produce a set of icons for a state that never actually ran — then
      # record that state as done.
      if [ "$current" != "$settling" ]; then
        log "modpack changed; waiting for it to settle"
        settling="$current"
        sleep "$POLL_INTERVAL"
        continue
      fi

      log "modpack settled, re-rendering"
      # The fingerprint is written only after a successful run, so a failure
      # here is retried on the next tick rather than being recorded as done.
      if render_all; then
        echo "$current" > "$FINGERPRINT_FILE"
        previous="$current"
        log "done — reload the admin panel, no API restart needed"
      else
        echo "!! rendering failed; will retry in ${POLL_INTERVAL}s" >&2
      fi

      settling=""
      sleep "$POLL_INTERVAL"
    done
    ;;

  *)
    sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'
    exit 1
    ;;
esac

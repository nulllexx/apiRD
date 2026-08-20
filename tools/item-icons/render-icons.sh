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

MODS_DIR="${MODS_DIR:-/mods}"
CACHE_DIR="${CACHE_DIR:-/cache}"
MC_VERSION="${MC_VERSION:-1.21.1}"
ICON_SIZE="${ICON_SIZE:-64}"
WORK="${WORK:-/work}"
POLL_INTERVAL="${POLL_INTERVAL:-300}"

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
  version_url="$(curl -fsSL "$manifest" \
    | php -r '$j=json_decode(stream_get_contents(STDIN),true);
              foreach($j["versions"] as $v){ if($v["id"]===getenv("MC_VERSION")){ echo $v["url"]; exit; } }')"

  if [ -z "$version_url" ]; then
    echo "!! Minecraft $MC_VERSION is not in Mojang's version manifest" >&2
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
  local ns="$1" out="$RENDER_OUT/$ns"
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

# Copy whatever was rendered into the cache, flattening any directory nesting.
#
# The cache expects <cache>/<version>/<namespace>/<item>.png, so a basename that
# is not an item id means the layout differs from what this assumes -- which is
# what `inspect` is for.
install_namespace() {
  local ns="$1" dest="$CACHE_DIR/$MC_VERSION/$ns" moved=0 file base
  mkdir -p "$dest"

  while IFS= read -r file; do
    base="$(basename "$file")"
    # Strip a namespace prefix if Renderchest emits one.
    base="${base#"${ns}"__}"
    base="${base#"${ns}"_}"
    cp -f "$file" "$dest/$base"
    moved=$((moved + 1))
  done < <(find "$RENDER_OUT/$ns" -type f -name '*.png' 2>/dev/null)

  echo "  $ns: installed $moved icon(s) into $dest"
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
  local ns
  for ns in $(namespaces); do
    echo "  rendering $ns"
    render_namespace "$ns"
  done
  log "installing into $CACHE_DIR/$MC_VERSION"
  for ns in $(namespaces); do
    install_namespace "$ns"
  done
}

case "${1:-}" in
  inspect)
    fetch_vanilla
    extract_mods
    ns="${2:-minecraft}"
    log "rendering only '$ns' so its output can be examined"
    render_namespace "$ns"
    log "output tree for '$ns' (first 40 entries)"
    find "$RENDER_OUT/$ns" -maxdepth 3 | head -40
    log "file count by extension"
    find "$RENDER_OUT/$ns" -type f | sed 's/.*\.//' | sort | uniq -c
    echo
    echo "Check that the PNG basenames read like item ids (diamond_sword.png)."
    echo "If they do, run: render-icons all"
    ;;

  all)
    render_all
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

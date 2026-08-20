# Renderchest in a throwaway container.
#
# Kept out of the API image on purpose: rendering icons is a batch job run when
# the modpack changes, not something the server does. Putting PHP and ImageMagick
# into the runtime image would add a large attack surface to a container that is
# exposed to the internet, for a task it never performs.
FROM php:8.3-cli

# libmagickwand-dev is installed and deliberately NOT purged afterwards.
#
# The usual trick is to drop the -dev package once the extension is compiled and
# reinstall only the runtime library, but that means hardcoding a versioned
# package name (libmagickwand-6.q16-6 and friends) which differs between Debian
# releases and vanishes the moment the PHP base image is rebased -- which is
# exactly how this broke the first time. This is a batch tool that runs when the
# modpack changes; a few hundred megabytes of headers it never reads costs
# nothing, and a build that cannot break on a base image bump is worth far more.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      libmagickwand-dev unzip ca-certificates curl git \
 && pecl install imagick \
 && docker-php-ext-enable imagick \
 && rm -rf /var/lib/apt/lists/*

# Fail the build here rather than at midnight when the modpack changes: an
# extension that compiled but does not load would otherwise only surface as a
# renderer that silently produces nothing.
RUN php -r 'exit(extension_loaded("imagick") ? 0 : 1);'

# Silence deprecation notices, and only those.
#
# Renderchest 2.10.1 predates PHP 8.3, and its triangle rasterizer does an
# implicit float-to-int conversion per scanline. That is one notice per scanline
# per triangle per item -- hundreds of thousands of lines for a full run, which
# buries the progress output and costs real time, because every line is written
# through the container's log pipe.
#
# Warnings, errors and fatals are deliberately left on: the version-mismatch
# crash that this pipeline has to be able to report is a fatal, and must stay
# visible.
RUN printf 'error_reporting = E_ALL & ~E_DEPRECATED\n' \
      > /usr/local/etc/php/conf.d/zz-quiet-deprecations.ini

COPY --from=composer:2 /usr/bin/composer /usr/bin/composer

# Pinned to 2.x for a hard compatibility reason, not out of caution.
#
# Renderchest finds items by scanning a directory, and which directory changed
# with the game: v2.x scans `assets/<ns>/models/item/`, which is where item
# models live up to 1.21.3. v3.0.0 switched to `assets/<ns>/items/`, the item
# definition format introduced in 1.21.4.
#
# Point v3+ at 1.21.1 assets and that directory simply does not exist, so it
# finds zero items, renders only its two internal placeholders, writes a
# stylesheet and exits 0 -- a silent success that installs nothing. That is
# exactly what happened here.
#
# So this must track the SERVER's version, not the newest release.
#
# Pinned exactly rather than as a range, because the range is not safe in either
# direction. `^2.4` resolves to 2.12.2, which is built for 1.21.4 snapshots and
# dies on 1.21.1 with:
#
#   TextureResolutionException: Cannot resolve texture locator
#   minecraft:trims/items/boots_trim_resin
#
# -- the resin armour trim, which does not exist before 1.21.4. 2.10.1 (Sept
# 2024, just after 1.21.1 shipped) predates the dynamic trim generator that
# demands it, and renders all 1721 vanilla items cleanly. Verified, not assumed.
#
#   Minecraft 1.21.4+       ->  ^5.1
#   Minecraft 1.21.1        ->  2.10.1   (matches MC_VERSION in docker-compose)
ARG RENDERCHEST_VERSION=2.10.1

WORKDIR /opt/renderchest
# The package publishes no `bin` entry, so composer never links anything into
# vendor/bin -- checking there is what broke the previous build. The CLI is a
# plain script at the package root, and it is checked for existence rather than
# for the executable bit, which a zip-based install does not reliably preserve.
RUN composer require "aternos/renderchest:${RENDERCHEST_VERSION}" --no-interaction --no-progress \
 && test -f vendor/aternos/renderchest/renderchest

COPY render-icons.sh /usr/local/bin/render-icons
RUN chmod +x /usr/local/bin/render-icons

ENTRYPOINT ["/usr/local/bin/render-icons"]

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

COPY --from=composer:2 /usr/bin/composer /usr/bin/composer

WORKDIR /opt/renderchest
RUN composer require aternos/renderchest --no-interaction --no-progress \
 && test -x vendor/bin/renderchest

COPY render-icons.sh /usr/local/bin/render-icons
RUN chmod +x /usr/local/bin/render-icons

ENTRYPOINT ["/usr/local/bin/render-icons"]

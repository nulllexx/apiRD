# Renderchest in a throwaway container.
#
# Kept out of the API image on purpose: rendering icons is a batch job run when
# the modpack changes, not something the server does. Putting PHP and ImageMagick
# into the runtime image would add a large attack surface to a container that is
# exposed to the internet, for a task it never performs.
FROM php:8.3-cli

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      libmagickwand-dev unzip ca-certificates curl git \
 && pecl install imagick \
 && docker-php-ext-enable imagick \
 && apt-get purge -y --auto-remove libmagickwand-dev \
 && apt-get install -y --no-install-recommends libmagickwand-6.q16-6 \
 && rm -rf /var/lib/apt/lists/*

COPY --from=composer:2 /usr/bin/composer /usr/bin/composer

WORKDIR /opt/renderchest
RUN composer require aternos/renderchest --no-interaction --no-progress

COPY render-icons.sh /usr/local/bin/render-icons
RUN chmod +x /usr/local/bin/render-icons

ENTRYPOINT ["/usr/local/bin/render-icons"]

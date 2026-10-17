#!/bin/sh
# Files written into a bind mount belong to whoever runs the container, and
# Docker's default is root. A root-owned database.db (with the -wal/-shm sitting
# next to it) cannot be read, checkpointed or replaced by the host user without
# sudo — which is how the 2026-10-13 corruption had to be repaired. A uid
# hardcoded here, or in .env, can only ever be right on one machine, so take it
# from the host mounts themselves: config.toml is bind-mounted from the checkout
# and the binary FATALs without it, so its owner *is* this host's answer. The
# operator configures nothing.
set -eu

if [ "$(id -u)" != 0 ]; then
    exec "$@"
fi

# SQLite creates a missing database file but never its directory, and compose
# creates a missing bind-mount source as root. Both paths below (drop and
# stay-root) need `data/` to exist, so create it before the uid decision.
mkdir -p /app/data

uid_gid=""
for probe in /app/config.toml /app/Rocket.toml /app/data /app/media; do
    owner="$(stat -c '%u:%g' "$probe" 2>/dev/null || true)"
    case "$owner" in
        "" | 0:*) continue ;;
    esac
    uid_gid="$owner"
    break
done

if [ -z "$uid_gid" ]; then
    echo "entrypoint: no host mount is owned by a non-root user, so this container" >&2
    echo "entrypoint: stays root and the files it writes (database.db, -wal, -shm," >&2
    echo "entrypoint: media) are root-only on the host. Fix once in the compose" >&2
    echo "entrypoint: directory, then restart:  chown -R \"\$(id -u):\$(id -g)\" data media" >&2
    exec "$@"
fi

# `-R` is for a database left root-owned by an earlier rootful run: SQLite
# rewrites the file in place and cannot do that without write permission on the
# file itself, which only the directory ownership would not give it.
if [ "$(stat -c '%u:%g' /app/data)" != "$uid_gid" ]; then
    chown -R "$uid_gid" /app/data
fi
# Same for media, but only when its root is root-owned: the app creates the
# media/<id>/ subdirectories, while files inside an ownable directory are
# replaced through a temp file + rename, which needs the directory, not the file.
if [ -d /app/media ] && [ "$(stat -c %u /app/media)" = 0 ]; then
    chown -R "$uid_gid" /app/media
fi

exec setpriv --reuid="${uid_gid%%:*}" --regid="${uid_gid##*:}" --clear-groups "$@"

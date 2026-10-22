# Deploy on a host

On a host, `zoe serve` is one static binary under systemd or supervisor. The host needs no Python. The directory [`deploy/`](https://github.com/adeweb-be/ZOE/tree/main/deploy) of the repository holds the files of this page. `just deploy check` tests them.

| File | What it is |
| --- | --- |
| [`deploy/zoe.toml`](https://github.com/adeweb-be/ZOE/blob/main/deploy/zoe.toml) | an example of the configuration file |
| [`deploy/systemd/zoe.service`](https://github.com/adeweb-be/ZOE/blob/main/deploy/systemd/zoe.service) | the systemd unit |
| [`deploy/supervisor/zoe.conf`](https://github.com/adeweb-be/ZOE/blob/main/deploy/supervisor/zoe.conf) | the supervisor program |

## Prepare the host

1. Build the static binary and install it as `/usr/local/bin/zoe` (see [Install](install.md#install-the-binary-on-a-host)).
2. Create the system user `zoe`.
3. Install `deploy/zoe.toml` as `/etc/zoe/zoe.toml`, and edit it.

The example file:

```toml
path = "/var/lib/zoe"
listen = "127.0.0.1:8100"
admin-listen = "127.0.0.1:8101"
shutdown-timeout = "30s"
log-format = "json"
```

- `path` is the store of the storage name `1`. Its blobs go to `/var/lib/zoe/blobs`. Add stores with `[storages.NAME]` tables.
- `listen` is the ZEO port. Put it on the private address of the host that the ZEO clients reach.
- `admin-listen` is the admin port, with `/healthz`, `/readyz` and `/metrics`. Put it on a private address, never on a public one.
- `shutdown-timeout` is the longest time of a stop. The units wait longer before they send SIGKILL.

[The configuration file of zoe serve](../reference/config-file.md) gives every key.

## systemd

```sh
cp deploy/systemd/zoe.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now zoe
```

The unit runs `/usr/local/bin/zoe serve --config /etc/zoe/zoe.toml` as the user `zoe`. systemd creates the state directory `/var/lib/zoe` for that user. The logs go to the journal: `journalctl -u zoe`.

- `TimeoutStopSec=45`: the stop on SIGTERM takes at most `shutdown-timeout`, 30 seconds. systemd sends SIGKILL only after 45 seconds. If you raise `shutdown-timeout`, raise `TimeoutStopSec` above it.
- `Restart=on-failure`: systemd restarts the server when it fails. A stop on SIGTERM exits with 0, which is not a failure.
- The unit hardens the service: the server writes only to its state directory. If you put a store or a blob directory elsewhere, add that directory to `ReadWritePaths=`.

## supervisor

1. Create `/var/lib/zoe` and `/var/log/zoe`, owned by the user `zoe`.
2. Copy `deploy/supervisor/zoe.conf` into the include directory of supervisord, for example `/etc/supervisor/conf.d/`.
3. Run `supervisorctl update`.

The program waits 45 seconds after SIGTERM before SIGKILL (`stopwaitsecs=45`). It restarts the server when it exits with a code other than 0. The log goes to `/var/log/zoe/zoe.log`: stdout holds the lines `admin` and `listening`, and stderr the logs.

## Check the server

```sh
curl http://127.0.0.1:8101/readyz
```

It answers `1: ready` with the status 200 when every store is open. Then connect the instances (see [Connect clients](clients.md)), and scrape the metrics (see [Monitoring](monitoring.md)).

## Back up

`zoe backup` needs the store free, so stop the server for the backup (see [Back up and restore](backup-restore.md)):

```sh
systemctl stop zoe
sudo -u zoe zoe backup --path /var/lib/zoe --to /var/backups/zoe
systemctl start zoe
```

Create `/var/backups/zoe` for the user `zoe` first. Run the backup as that user: the dumps then belong to it, and a repair of the store keeps the owner of the store. The clients reconnect when the server is back. An online backup through the server is not built yet.

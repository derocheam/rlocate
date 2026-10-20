# Running rlocate as a service (Linux / systemd)

These units run the query server on boot and rebuild the index hourly. Run the
commands as root.

## 1. Build and install the binaries

```sh
cd rlocate
cargo build --release
install -m 0755 target/release/rlocate          /usr/local/bin/
install -m 0755 target/release/rlocate-updatedb /usr/local/bin/
install -d -m 0755 /var/lib/rlocate
```

## 2. Create the token env file (not world-readable)

```sh
install -d -m 0755 /etc/rlocate
printf 'RLOCATE_TOKEN=%s\n' "$(openssl rand -hex 24)" > /etc/rlocate/server.env
chmod 600 /etc/rlocate/server.env
cat /etc/rlocate/server.env   # note the token for your clients
```

## 3. Build the index once (the server won't start without it)

```sh
systemctl daemon-reload   # after copying the units below
cp packaging/systemd/rlocate-*.{service,timer} /etc/systemd/system/
systemctl daemon-reload
systemctl start rlocate-updatedb.service   # first build, ~1–3 min for a whole disk
rlocate -S                                 # sanity check the path count
```

## 4. Enable the server and the hourly timer

```sh
systemctl enable --now rlocate-server.service
systemctl enable --now rlocate-updatedb.timer
```

Check status:

```sh
systemctl status rlocate-server.service
systemctl list-timers rlocate-updatedb.timer
journalctl -u rlocate-server.service -e
```

## How the hourly refresh reaches the server

`rlocate --serve` memory-maps the index **once at startup**, so it won't see a
newly built index on its own. The `rlocate-updatedb.service` therefore runs
`systemctl try-restart rlocate-server.service` after each rebuild (a restart
just re-opens the mmap — effectively instant). There's a sub-second window each
hour where the server is down.

## Querying it from your client

Set the same token and point `--host` at the box, or add it to your client's
`~/.config/rlocate/remotes.conf`:

```ini
[proxmox]
host = <proxmox-ip>:9999
token = <the token from server.env>
```

## Security notes

- The transport is **plaintext TCP**. `--listen 0.0.0.0:9999` exposes it on all
  interfaces — fine on a trusted LAN. Restrict it with a firewall, bind a
  specific interface, or front it with an SSH tunnel and `--listen 127.0.0.1:9999`.
- Keep `/etc/rlocate/server.env` at mode `600`; it holds the token.
- The server returns **file paths** (not contents) from the index to anyone with
  the token.

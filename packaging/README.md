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

## 2. Store the token as an encrypted systemd credential (recommended)

`systemd-creds` encrypts the token with a host-bound (or TPM-bound) key. systemd
decrypts it at service start into a private tmpfs file (`%d/token`, mode 0400,
readable only by this service) — never in plaintext on disk at rest, never in
the environment, never world-readable.

```sh
install -d -m 0755 /etc/rlocate
TOKEN=$(openssl rand -hex 24)
echo "token for your clients: $TOKEN"
printf '%s' "$TOKEN" | systemd-creds encrypt --name=token - /etc/rlocate/token.cred
```

The `rlocate-server.service` unit references it with
`LoadCredentialEncrypted=token:/etc/rlocate/token.cred` and passes
`--token-file %d/token` to the server.

> Note: a credential encrypted with the default host key only decrypts on *this*
> machine. To rotate the token, re-run the `encrypt` command and
> `systemctl restart rlocate-server`.

### Simpler alternative (plaintext file, root-only)

If you don't want encryption, a root-owned `0600` file also keeps the token out
of the environment and `ps` (better than an env var):

```sh
install -d -m 0755 /etc/rlocate
openssl rand -hex 24 > /etc/rlocate/token
chmod 600 /etc/rlocate/token
```

Then in `rlocate-server.service`, replace the `LoadCredentialEncrypted=` line and
the `--token-file %d/token` argument with `--token-file /etc/rlocate/token`.

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
token = <the token you generated in step 2>
```

## Security notes

- The transport is **plaintext TCP**. `--listen 0.0.0.0:9999` exposes it on all
  interfaces — fine on a trusted LAN. Restrict it with a firewall, bind a
  specific interface, or front it with an SSH tunnel and `--listen 127.0.0.1:9999`.
- The token is stored encrypted (`/etc/rlocate/token.cred`); it is only
  decrypted into a service-private tmpfs at runtime. If you used the plaintext
  fallback instead, keep that file at mode `600`.
- The server returns **file paths** (not contents) from the index to anyone with
  the token.

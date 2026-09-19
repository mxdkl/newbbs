# newbbs

A Discord-shaped BBS you reach over ssh. Rust, ratatui, SQLite.

Identity is your ssh key. The board is invite only.

## Run it

```sh
podman-compose up -d            # pulls ghcr.io/mxdkl/newbbs:latest
podman-compose up -d --build    # or build from source
```

Listens on port 2222.

## Add yourself

```sh
podman exec newbbs newbbs invite alice "$(cat ~/.ssh/id_ed25519.pub)"
podman exec newbbs newbbs grant alice sysop     # optional: make them an admin
ssh -p 2222 newbbs@<host>
```

## Admin console

`admin` has no ssh key, so the only way in as admin is from the server:

```sh
podman exec -it newbbs newbbs console
```

From there: `:invite`, `:grant <user> <role>`, `:mkchan`, `:motd <text>`.
`:help` lists the rest.

## Keys

`i` write, `esc` back, `:` command, `j`/`k` scroll, `gg`/`G` top/bottom,
`/` jump to a channel, group or DM.

## Splash art

```sh
cat art.ans | podman exec -i newbbs newbbs art -
podman exec newbbs newbbs motd "welcome"
```

UTF-8 text, with or without ANSI colour codes. `--reset` restores the default.

## Back it up

Everything lives in the `newbbs-data` volume, **including the ssh host key** --
lose it and every client gets a host-key-changed warning.

```sh
podman volume export newbbs-data > newbbs-backup.tar
```

# newbbs

A Discord-shaped BBS you reach over ssh. Rust, ratatui, SQLite.

Identity is your ssh key and the board is invite only: an admin adds your key,
you connect, you are you.

## Running it

```sh
podman build -t localhost/newbbs -f Containerfile .
podman volume create newbbs-data
podman run -d --name newbbs -p 2222:2222 -v newbbs-data:/var/lib/newbbs localhost/newbbs
podman exec newbbs newbbs invite alice "$(cat ~/.ssh/id_ed25519.pub)"
ssh -p 2222 newbbs@localhost
```

Or with compose (needs `podman-compose` or `docker-compose` on PATH). It uses
the same volume, so you can switch between the two freely:

```sh
podman compose up -d
podman exec newbbs newbbs invite alice "$(cat ~/.ssh/id_ed25519.pub)"
```

`podman logs newbbs` shows the log. The database lives at
`/var/lib/newbbs/newbbs.db` on the volume and holds everything, **including the
ssh host key** -- back up the volume, or clients will see a host-key-changed
warning after a rebuild.

## Running it locally

```sh
cargo run -- serve --ui          # ssh listener plus a TUI session on this terminal
cargo run -- invite bob bob.pub
cargo run -- motd                # show the splash message, or set it
cargo run -- art banner.txt      # set the splash art from a file
cargo run -- log --limit 20      # the event log, as JSON
cargo run -- snapshot            # render one frame to stdout, no tty needed
```

An ssh session is met by the splash -- art with the message underneath --
which waits for a keypress before the board appears. `serve --ui` skips it.
Admins can set the message from inside with `:motd <text>`; the art comes from
a file because it cannot be typed on one line. `--reset` on either puts the
built-in back.

The art file is UTF-8 text, with or without ANSI colour codes. Plain art is
drawn in the theme's accent colour; coloured art keeps exactly the colours it
was written with (`ESC[...m` -- 16-colour, 256-colour and RGB, foreground and
background). Cursor-movement codes are skipped rather than drawn, so classic
CP437 `.ANS` files from the scene will not render properly yet. Art wider than
the visitor's terminal is dropped, leaving the message on its own.

A fresh database starts empty: one `#general` channel, a `sysop` role, and an
`admin` account. `admin` has no ssh key and can never get one -- it is only
reachable from `serve --ui` on the server itself, so if admin is online,
someone is sitting at the box. Grant roles to other people from there with
`:grant <name> sysop`.

## Keys

Normal mode: `i`/`a` write, `:` command, `j`/`k` scroll, `gg`/`G` top/bottom,
`/` jump to a channel, group or DM. `:help` lists the rest.

## How it works

The event log is the source of truth: every change is an event appended to
SQLite, applied to the projection tables in the same transaction. Sessions
never touch the database -- they read and write through a bus that also
broadcasts each event to whoever is subscribed to its tags. Small payloads
travel whole; anything larger travels as a uuid the session resolves.

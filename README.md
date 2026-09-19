# newbbs

A Discord-shaped BBS you reach over ssh. Rust, ratatui, SQLite.

Identity is your ssh key and the board is invite only: an admin adds your key,
you connect, you are you.

## Running it

```sh
podman build -t localhost/newbbs -f Containerfile .
podman volume create newbbs-data
podman run -d --name newbbs --network host -v newbbs-data:/var/lib/newbbs localhost/newbbs
podman exec newbbs newbbs invite alice "$(cat ~/.ssh/id_ed25519.pub)"
podman exec newbbs newbbs grant alice sysop            # make them an admin
ssh -p 2222 newbbs@localhost
```

Administering a board that is already running:

```sh
podman exec -it newbbs newbbs console                  # a TUI session, no second listener
cat art.ans | podman exec -i newbbs newbbs art -       # set the splash art from outside
```

`console` shares the database but has its own event bus, so admin commands
take effect immediately while messages from connected users only appear after
`:reload`.

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
cargo run -- serve               # the ssh listener
cargo run -- invite bob bob.pub
cargo run -- console             # the admin TUI (this is the only way in as admin)
cargo run -- grant bob sysop     # or --revoke
cargo run -- motd                # show the splash message, or set it
cargo run -- art banner.txt      # set the splash art (a file, or - for stdin)
cargo run -- log --limit 20      # the event log, as JSON
cargo run -- snapshot            # render one frame to stdout, no tty needed
```

An ssh session is met by the splash -- art with the message underneath --
which waits for a keypress before the board appears.
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
reachable from `newbbs console` on the server itself, so if admin is online,
someone is sitting at the box. Grant roles to other people from there with
`:grant <name> sysop`.

## Keys

Normal mode: `i`/`a` write, `:` command, `j`/`k` scroll, `gg`/`G` top/bottom,
`/` jump to a channel, group or DM. Scrolling past the top pages in older
history. `:help` lists the rest -- admin commands only appear for admins.

Roles are cosmetic except `sysop`, which is the one that grants power:
`:roles` lists them, `:mkrole <name> <#rrggbb> [priority] [hoist]` creates one,
`:rmrole <name>` removes it from everyone who holds it, and
`:grant <user> <role>` hands it out. The highest-priority role colours a name;
hoisted roles get their own section in the member list.

## How it works

The event log is the source of truth: every change is an event appended to
SQLite, applied to the projection tables in the same transaction. Sessions
never touch the database -- they read and write through a bus that also
broadcasts each event to whoever is subscribed to its tags. Small payloads
travel whole; anything larger travels as a uuid the session resolves.

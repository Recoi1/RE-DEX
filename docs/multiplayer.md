# Multiplayer (two trainers on one map)

RE:DEX players meet through a `redex-world` server. Every player on your map is drawn as an NPC (a "puppet") in the
game's own move-model pool, made and moved by the game's functions. A puppet walks where the other player walks, goes
when they leave the map or disconnect, and never reaches your save. X and Y players share one world (the Kalos region).

- The protocol, the rooms and the Steam login: `crates/redex-world`, `crates/xr-net` (see their module docs).
- How puppets are made, moved and kept out of saves: `docs/re/x-field-move-models.md`.
- The game needs an `xgame.dll` built with the puppet hooks (`functions/<build>/hooks.txt`). Without them it plays
  normally but shows nobody, and the log says `WARNING: puppets: xgame.dll was built without the puppet hooks`.
- With a hooked DLL the hooks fire in every run, offline too. With nobody online they are pass-throughs: each one
  calls the game's own function and does nothing else, so offline play and the playthrough hashes stay the same.
- redex checks the DLL was made with its hook list: `xr_game_info` carries a hash of hooks.txt (ABI 6), and a DLL made
  with another list is refused ("made with another hook list ... rebuild it with xr-recomp").

## Identity

Online, a player is their Steam account (SteamID64), proven to the server with a Steam auth ticket (Steamworks app 480,
Spacewar). The Steam client must be running and logged in; without it the game says "Start Steam to play online" and
plays offline. `steam_api64.dll` must sit next to `redex.exe`.

Only one Steam account can run on a PC. For a second instance on the same PC there are **dev ids**: `--dev-id` logs in
with a random id made once in `<data>/user/redex-id`. Only a server started with `--dev` accepts them, and such a server
listens on localhost only. `--dev-id` and `--dev` exist only in an `mp-tests` build (the MP test harness, see Testing):
a release has neither.

## The server

```
redex-world --p2p --allow FILE [--name TEXT] [--query-port UDP] [--data DIR]
redex-world --dev [--bind ADDR:PORT]          (an mp-tests build only)
```

- One of `--dev` or `--p2p` is needed; without either the server does not start.
- **Invite-only until moderation (M8).** `--p2p` refuses to start without `--allow FILE`: one account a line, the
  only accounts let in. `<data>/bans.txt` (same form) keeps accounts out. A line is a SteamID64, `STEAM_0:X:Y`,
  `[U:1:N]` or a `steamcommunity.com/profiles/<id64>` URL; a BOM, spaces, blank lines and `#` comments are fine, and a
  line that names no account is warned about on the server's stderr with its line number (`WARNING: <file> line N
  names no Steam account`) and counts for nothing. Both are read at every Steam login, right after the ticket is
  verified, and every 30 s for each connected Steam player (parsed again only when the file changed): a ban, or leaving
  the allow list, disconnects a player within 30 s. A list that can't be read refuses everyone (a missing `bans.txt`
  bans nobody). A refused player gets `AUTH` with the reason ("You're not on
  this world's list; ask its host", "you are banned from this world"), shown to them as "Cannot join: ...". Spacewar accounts are free, so a
  SteamID ban is weak: the allow list is the real gate until M8.
- `--dev` (an `mp-tests` build only): TCP on `127.0.0.1:7777` (or `--bind`, localhost only). Dev ids and Steam logins. The server starts an
  anonymous Steam game server to check tickets; that needs `steamclient64.dll`, `tier0_s64.dll` and
  `vstdlib_s64.dll` from the Steam client or SteamCMD next to it. If Steam's game server cannot start, a dev server
  runs anyway with dev ids only.
- `--p2p`: listens over Steam's relays (SDR) instead of TCP; players join with `--world steam:<id64>` (the id is
  printed at start and changes at every restart). Steam logins only; it cannot be combined with `--dev`.
- **Over TCP a Steam ticket is not bound to the server.** Whoever runs a TCP server could pass your ticket on to
  another server and log in there as you. Over `steam:` the ticket names the server it is for. So a TCP server takes
  Steam tickets only with `--dev` (localhost), and anything that is not your own machine goes over `steam:`.
- A Steam account can log in 3 times a minute; a fourth login gets a refusal the client retries (`FLOOD`).
- Protocol v5 (a v4 client is refused with "update RE:DEX"). Names are cut to 16 characters (control and bidi characters
  stripped); a name that reads as Admin, Moderator, RE:DEX/Redex, Nintendo, Game Freak, Pokémon, System or Server
  becomes "Trainer": compared in lower case without spaces and punctuation, with full-width letters as ASCII and common
  look-alikes as the letter they pass for (0 o, 1 l | ! i, 3 e, 4 @ a, 5 $ s, 7 t, Cyrillic а е о р с х у і ѕ к м т).
  Two players may still share a name (duplicates are not marked yet). The appearance at `Hello` and in `AppearanceUpdate` must
  pass `Appearance::from_bytes` (the session's generation, every field in range), as a `CharacterPut` look does: a bad
  one is dropped (empty at `Hello`, ignored in an update). A `Pos` with an unknown mode is dropped.
- A `Pos` more than 4 tiles per 100 ms (at least 100 ms' worth) from the last relayed one is dropped, except the first
  after `Entered`; the allowance grows with the time since, so a warp inside a map comes through a moment late. That is
  the only movement check (there is no world simulation), so a report's trails are what the clients claimed and its
  line says `"trails":"client-claimed"`.
- Whispers go to a player on the sender's map and channel, or as a reply to one who whispered the sender in the last
  10 minutes (from any map); anything else is dropped silently (session ids are small numbers anyone could count
  through). 5 per 10 s per sender, the excess dropped silently; text cut to 120 characters.
- `Peer` carries the player's verified SteamID64 (0 for a dev id), which "Add friend" opens Steam's overlay on. Everyone
  on your map can see it, as in any Steam game where you play together.
- Reports (`Report{session, steam_id, reason, note}`, 3 a minute per reporter; the note up to 200 characters of any
  width, a u16-length string on the wire) are dropped unless `steam_id` is the session's account (0 for a dev id: session
  ids start over in every server, so a stale one never lands on another player); the rest append one line to `<--data dir>/reports.jsonl`
  (default `redex-world-data/` next to the exe): both players' Steam IDs (dev ids on a `--dev` server), the time, the reason,
  the note, both players' last 30 s of positions and the target's last 20 whispers. Metadata only. Past 16 MB the file
  becomes `reports.jsonl.1` (an older one goes), so reports never take more than 32 MB; one connection writes at a
  time, so a rotation never loses a line.
- `--name TEXT` (default "RE:DEX world") and `--query-port UDP` (default 27016): a `--p2p` server advertises itself in Steam's
  server list (app 480) with game tags `redex,v5,kalos`. Players find it by the tags and the SteamID; the name shows only when
  Steam can reach the query port over UDP (a public server needs UDP 27016 open; a second `--p2p` server on the same PC needs its own `--query-port`). A `--dev` server is not listed.
- A player who disconnects cleanly (quits, or the connection closes) is gone from the others' screens within about
  2 s. A connection that dies without closing (a cable pulled, a half-open TCP connection) keeps its puppet up to 5 s:
  the server's and the client's timeout for a silent player.
- `--sim MIN,MAX,LOSS` (only in a build with `--features sim`): delays every send by MIN..MAX ms and drops LOSS% of the
  relayed positions, for tests.

## What a world refuses (the MP lockdown)

The client keeps dev and test tools out of worlds; the server's checks above are the real fence (players build
`xgame.dll` themselves, and saves and mods are local files).

- **Dev and test inputs.** `Joiner::start`, which every join goes through (the boot's `--world`, Browse servers,
  Direct connect, Steam's "Join game"), refuses while any is on and the game plays offline, with a toast in a window
  (headless: a log line, `world: not joining <addr>: Online is off while test options are on (...); playing offline`):
  `--headless`, `--script`, `--press`, `--deterministic`, `--make-preview`, `--room`/`--room-server`, and any of
  `XR_STATE_*`, `XR_SET_FASHION`, `XR_EXPLORE*`, `XR_FAKE_*`, `XR_MP_*`, `XR_RAILPOS`. An `mp-tests` build allows them
  (the pair tests). `--dev-id`, `--room` and `--room-server` exist only in such a build: without a room the game's own
  local wireless (PSS trades, battles, Download Play) is off, and those trades must never bypass the server.
- **Mods that change the game.** A mod that changes a RomFS file off the cosmetic list (`xr-hle/src/cosmetic.rs`:
  the GARCs of Pokémon, player and trainer models and the menus' pictures), the exheader or the code
  (`System::game_modded`, the mod's name): "Turn off mods to play online: <mod> changes the game (skins and texture
  packs are fine)". Texture packs and skins on the list play online; other players never see them (see
  [mods.md](mods.md)).
- **An edited game file.** "Add your game" hashes the dump anyway; its SHA-256 is kept with the file's size and time in
  `<data>/dumps/verified.txt`. At a world's door the dump (`--cci`) is taken as that build while its size and time are
  the same; otherwise it is hashed again once (the answer kept), and one that is no longer a known build's (a RomFS
  rebuilt into the .3ds by an editor or a randomizer) is refused like a mod: "This game file was changed since it was
  added; online is off". A dump never added is hashed at its first join. A normal launch hashes nothing.
- **Speed.** While joined to a world (connected or not: a dropped connection is no way round it) there is no fast
  forward and the speed is 100% at most (slower is fine); a toast says "Speed is 100% in a world" once.
- **Ignore.** The player menu's Ignore drops that player's whispers here for the rest of the run: by Steam account
  (from any session or map; the client keeps each session's account as its `Peer` announced it, for the connection),
  or by world and session for a dev id; Stop ignoring lets them through. Not saved: a friend system's block list will be.
- **The release guard** (`check-release.ps1`) refuses a `redex.exe` or `redex-world.exe` built with `mp-tests` (its
  `--help` text says `MP-TESTS BUILD`), so a test build can't ship by accident.
- **Rule: no Pokémon, items or money on the wire yet.** No message carrying PK6 data, items or money may be added to
  the protocol before the legality daemon, the registry and escrow exist (M3). Edited or imported saves join freely
  today and are harmless only because nothing Pokémon-shaped crosses the server; a client gate can't stop an edited
  save, so that fence is server-side only.

## Two instances on one PC

1. Start the server: `redex-world --dev`.
2. Player A, a window with your Steam account:
   `redex --cci <game> --build <dataA>/build/x-usa-v0.2.0 --world 127.0.0.1:7777 --name A`
3. Player B, a second window (or `--headless`) with a dev id and **its own data dir**:
   `redex --cci <game> --build <dataB>/build/x-usa-v0.2.0 --world 127.0.0.1:7777 --name B --dev-id`

Each instance needs its own data dir (its own save, its own `redex-id`). Walk both players onto the same map: you see
each other within about a second. In a window, the settings' world address and name are used when `--world` and
`--name` are not given (`%APPDATA%\redex\settings.toml`; `REDEX_SETTINGS` points a window at another file).

Between two PCs: run `redex-world --p2p` on one, and both players join with `--world steam:<id64>`; no port needs to
be opened. Not over plain TCP: a Steam ticket sent over TCP is not bound to the server (see "The server"), so a TCP
server takes logins on localhost only.

## Finding a P2P server

**The query port of a P2P server.** Players find a `--p2p` server through Steam's master list (tags `redex,v5,kalos`), which
needs nothing opened. Its name, player count and ping come from a UDP query to `--query-port` (default 27016): if Steam
cannot reach it (a router without that port forwarded, a firewall), the server is still listed and joinable, as "Unnamed
world" with no ping. Two P2P servers on one PC need different `--query-port`s.

## Characters on the account (protocol v5)

A character (made in the start screen's creator: a name and a look, `<data>/characters.json`, its saves in
`<data>/characters/<id>/user`) belongs to the player's account, not to one PC or one game:

- At every login (`Welcome`) the game puts each of its characters (`CharacterPut`), and later changes as they happen: a
  look the game changes (a boutique, a salon) is kept in `characters.json` and goes up at once. A character made
  offline goes up at the next login.
- The server keeps at most 8 per account in `<--data dir>/accounts/<key>.bin` (the key is the Steam id or the dev id) and
  answers each put and each login with the account's list (`Characters`). For each id the newer `changed` wins (a tie
  keeps the PC's own). The game merges the list into `characters.json`, so a character made on X shows on Y after Y
  logs in, and a newer copy replaces an older one at the next start.
- The server checks every put: at most 16 per 10 s per session, id not 0, `changed` at most a day ahead, the look
  through `Appearance::from_bytes`, the name as `--name`'s. A broken account file is kept aside
  (`.bin.bad-<ms>`), never saved over; the same holds for `characters.json` (`characters.json.bad-<ms>`), which is
  written through a temporary file of the process's own, synced to disk before the rename.
- The game checks the list that comes back the same way (a hostile world can send anything): a copy whose `changed` is
  more than a day ahead of this PC's clock is dropped, so no world can win every merge for ever. The start screen's
  **Remove character** (after a confirm) takes a character off the list and remembers its id in `characters.json`
  (`removed`), so the account's copy does not bring it back; its saves stay in `<data>/characters/<id>`, and the
  screen says where. The server's account keeps the removed copy (there is no delete on the wire yet).
- `redex --character <id>` plays as that character: its saves only (backed up first, to
  `<data>/characters/<id>/backups/<title>/`), and its look in the game (a new game takes its gender, name and look;
  Continue its look, on a save of its gender). Online, the other players see that look on its puppet.

## What other players look like (M1, outfits and modes since M1b)

- A boy or a girl in the other player's own outfit (the 19-byte appearance: gen 6, version 1, gender, the save's 16
  fashion bytes; sent at login from the save, then again whenever the field shows a new one, e.g. after a boutique).
  The bytes are checked against the game's own ranges before anything touches the game (`Appearance::from_bytes`):
  an outfit that fails shows the plain look of the bytes' gender, missing or malformed bytes the plain boy. A girl
  shows as a plain boy where the girl model is not loaded. An outfit needs a free dress-up slot (2 are left to the
  game) and 160 KB more model heap, and its parts load one outfit at a time (a loader busy for 10 s lets the next ones
  come plain); without them the puppet wears the plain look. A puppet's outfit never shares the game's slot for the
  player's (a twin in your exact outfit loads its own). A changed outfit is a new puppet (deleted, then made again a
  second later).
- **Girls.** A girl's outfit loads the game's *player* part set (header byte 3 = 1): on the NPC set a boy's
  outfit draws fine, but a girl's long hair is drawn as a brown plank and short hair not at all. The player part set
  takes one of the game's two player heaps (`[0x5E9EE4]+0x2C` and `+0x30`) and your own outfit holds the
  other, so one girl outfit is on the player set at a time: a second girl in the same outfit shares it, a girl in
  another outfit, or in your exact outfit (a twin: the game's own slot is never shared), wears hers on the NPC set:
  her clothes, skin and eyes are right, long hair is the plank. Each outfit keeps the set it was
  asked for on when it is asked for again after a battle or released. While a shared girl outfit's parts are gone
  (after a battle) and no player heap is free, a new girl in it waits rather than load into a null heap. The girl model
  has no body of its own (it is all dress-up parts), so a girl with no outfit at all (a stuck loader, no free slot or
  heap, a boy shown on a girl stand-in) is not made: she would draw only her shadow, the item she holds and her name.
  Picture: `art/ref/girl-puppets-before-after-c1.png` (three girls in the bedroom: player set, NPC set, twin).
- A girl puppet is a girl only where the girl model is loaded, which in practice means the local player is a girl
  too; with a boy local player a girl shows as a plain boy, so girl outfits (and the player heaps) come into play only
  between girls.
- Modes: walking and running as before; on the bike and on skates they run in place at the bike's or the skates' speed
  (the bicycle and the skating pose are not drawn on a puppet yet); a surfing player is not shown until surfing is
  traced.
- The 8 nearest players on the map are shown (the field's model heap limits it).
- They walk and run where the other player goes: SetTransform places them, and they step in place (the game's action
  codes 26 and 27) while they move, standing when the other player stops.
- Puppets cannot be talked to and do not block: you walk through them.
- What the server relays is not trusted: a position that is not a finite number is dropped, x and z are kept within
  32 tiles of you and never below 0, and the height within 4 tiles of yours. A player whose puppet was deleted waits
  a second before it is spawned again, and puppets are spawned one every 4 frames, so a server that announces and
  drops players over and over cannot make the game create and delete models every frame.
- The client keeps at most 64 other players on your map (a server could announce any number), and filters their names
  as the server does (control, bidi and zero-width characters): a hostile server can send any text.

## In the window (M1b): names, the Players list, whispers, reports, servers

- **Names** over the other players' heads, on the top screen only, scaled with the window, fading in and out (and near
  the screen's edges). A name shows only while that player's puppet is drawn (a dressed one once its outfit's parts are
  loaded: `Puppets::drawn`) and the field camera is up, so none shows in a battle or the full-screen menus (DllField is
  unloaded there, `field::camera` is None) and none outlives its puppet (a quarter-second fade at most). Configure >
  General > "Show player names" turns them off. Clicking a name opens the player menu. The pause menu (X) keeps them:
  the top screen there is the live field. Known limit: a door's iris fade does not hide them until the field closes (a
  fraction of a second), as no cheap fade signal is known (reading the screen back every frame would cost a GPU sync).
- **Players nearby**: a tap on Tab (holding Tab still fast forwards) or the controller's left stick click, which no
  console button is on (Configure > Controls > "Players list" rebinds both; a controller button the console layout uses, and Home, work the
  menus or the game and are refused, as is a console binding onto the Players button; only buttons like the stick clicks, or
  non-console inputs, can open it) lists the players on your map, the nearest
  first, with their version (X blue, Y red). Up/down, Enter or A, Esc or B work it and every panel below; while one is
  up the game runs on but gets no input, and every button still held when the last one closes (the A or B that closed
  it) stays away from the game until it is let go.
- **The player menu**: Whisper (a box: 120 characters, at most 5 every 10 s, as the server allows), Add friend (Steam's
  overlay `friendadd` on their SteamID; a toast says so when the overlay is not there, as for a redex.exe Steam did not
  start, and for a dev id), Report (Cheating, Harassment or spam, Offensive name, Something else, and an optional note
  of up to 200 characters; a toast confirms once the client took it). Trade, Battle and Invite to team come in later
  milestones. With a controller alone the whisper box asks for Steam's gamepad keyboard (Big Picture's), and says
  "Connect a keyboard to type" where Steam cannot show it. A panel about a player (menu, whisper box, report card, a
  whisper-log reply) belongs to the world it was opened in: joining another world closes it, and a log line from the old
  world cannot be answered.
- **Whispers** that come in show as a toast and in a log at the bottom left (Multiplayer > Show whispers pins it;
  clicking a line answers). Names and texts from the server are cleaned of control, bidi and zero-width characters
  (`xr_net::client::clean`): a hostile server can send anything.
- **Multiplayer > Browse servers...** asks Steam's internet server list (app 480) for `gametagsand=redex,v5`, checks
  the tags again, and lists name, players and ping; a double-click (or A) joins `steam:<server id>`. It asks again on
  every open, every 20 s while open and on Refresh, and keeps a server for 3 minutes after an answer missed it: Steam
  lists a new server 40 s to 2 minutes after it starts, and not in every answer. A server whose query port Steam cannot
  reach shows as "Unnamed world" and the end of its id, with no player count or ping ("—"). "Direct connect" takes
  `steam:<id>` or `host:port` (a Steam login goes over TCP to this PC only: elsewhere use `steam:<id>`; a remote `host:port` set at boot with Steam running is
  dropped with a notice and the game plays offline, it never blocks the boot). The world you
  are in stays until the new one logs you in; one that does not within 15 s is given up ("Couldn't reach that world")
  and you stay where you were. Then the old world's puppets go as its players vanish. Steam is started at the first
  Join or search if it was not up, and tried again at the next.
- **Steam rich presence** while online: `status` "In Kalos" (map names would need the game's text) and `connect`
  `steam:<server id>` (only for a `steam:` world), so "Join game" in a friend's list works while redex runs (Steam's
  join callback); `redex ... +connect steam:<id>` joins at start like `--world`. Both take `steam:<digits>` only: a
  `host:port` there would come from anyone's rich presence and get our Steam ticket over TCP, unbound to that server.
  Steam starts Spacewar, not redex, for a
  friend whose redex is not running: app 480 is not our own app id.

## Friends: Steam friends, invites, chat

The friends list is the player's Steam friends. No
RE:DEX server is involved: friends' chat and invites go game to game over Steam's networking messages, relayed by
Steam like the world connection.

- **Who's in RE:DEX.** A window starts Steam at once (not with `--dev-id`; offline play goes on if Steam is closed) and,
  once Steam is up (also when it starts later), sets the rich-presence key `redex` = the protocol version, beside `status` ("<world name> · In Kalos" online,
  the name when Steam's list gave it; "Playing offline" otherwise) and `connect`. A friend is in RE:DEX when the `redex`
  key holds this protocol's version and their game is app 480 (the key alone when Steam can't say which game); another
  version shows as "Another RE:DEX version". The list is read again every 10 s (presence is asked only while the tab
  is open). The start screen sets no
  presence: nothing there could take a friend's chat or invite.
- **The Friends tab**, beside Players nearby (Left/Right, or a click): friends running RE:DEX first, with what their
  presence says, then friends online in Steam only (greyed, a hollow dot: "Not in RE:DEX"; chat and invites to them go
  through Steam itself, never through Spacewar). Presence is asked when the tab opens and every 10 s while it's open.
  A friend's menu: Chat, Invite to my world (only while you're logged in to a `steam:` world), Ignore / Stop ignoring.
- **Invites.** An invite carries the inviter's world name (32 characters at most) and its `steam:<server id>`; one
  that names anything else does not decode. The friend sees "<name> invites you to <world>", Join / Not now (A / B),
  Not now focused; A / Enter / Join count only after its first half second, and while a chat or whisper box is up its
  keys stay the box's. A newer invite replaces the prompt. Join is the usual join (`Windowed::join` -> `Joiner::start`), so every check
  applies: dev and test inputs, mods, an edited game, the world's allow and ban lists (its refusal shows as usual,
  "You're not on this world's list; ask its host" for the allow list). Not now tells the inviter "<name> can't come
  right now". Steam's own game invite is never used (it would launch Spacewar for someone without RE:DEX).
- **Chat**, in a window at the bottom right with a tab per friend, unread counts (also on the Friends tab and the
  friend's row) and a toast per message, as whispers. Text is cleaned like a whisper (control, bidi and zero-width
  characters out, 120 characters) and sending is limited to 5 every 10 s. It works across worlds and offline while both
  run RE:DEX; when the friend leaves, their tab stays and the box says "<name> isn't in RE:DEX". This run's lines only
  (Steam keeps no in-game chat). With a controller alone, A types with Steam's gamepad keyboard, A again sends. A line
  that didn't go is marked "not sent" with why (not in RE:DEX, sending too fast, the connection failed, Steam's own
  error); when Steam gives up on a friend's session, the last line sent to them is marked and a toast says so.
- **Ignore** (the player menu's, now also the friend menu's) drops that Steam account's whispers, chat and invites,
  and is kept per own Steam account in `<data>/user/ignored-<own steamid>.txt` (one SteamID64 a line, written whole
  through a temporary file). Ignores made before Steam was up are kept and saved with it. A file with a line that isn't
  an id, or not UTF-8, is kept aside as `<name>.bad-<time>` and its readable ids count; one that can't be read at all
  is not written over that run. Ignored friends stay listed, marked "ignored" (unfriending stays Steam's job).
- **The transport** (`xr_net::friends`): reliable frames (`redex_proto::FriendMsg`, versioned, 512 bytes at most) on
  channel 7. The link starts only once the window read the Ignore list, and a session is accepted only from a Steam friend who isn't
  ignored; frames from anyone else, past 10 per
  10 s from one friend, or that don't decode are dropped.
- **Tests.** `FakeFriends` stands in for Steam in tests and `mp-tests` builds: `XR_FAKE_FRIENDS=ID:NAME[:STATUS];...`
  (a STATUS means running RE:DEX) and `XR_FAKE_FRIEND_SAYS=FRAME:ID:chat:TEXT|FRAME:ID:invite:steam:<id> WORLD|
  FRAME:ID:decline;...`. Both are dev inputs (`XR_FAKE_`): a release run with them stays out of worlds. A headless
  run answers a fake chat with `XR_MP_FRIEND_REPLY` and joins a fake invite (through `Joiner::start`); see
  `tests/exec.rs` `a_fake_friends_chat_arrives_and_is_answered` and `invite_join_hits_lockdown`, and the frontend's
  `online::tests` for the tab, chat, prompt and Ignore. `friends_screenshots` (ignored; `XR_UI_SHOTS=DIR`) renders the
  panels to PNG.

### Manual Steam check (two PCs, two Steam accounts that are friends)

The real Steam path can't be tested on one PC (one Steam account per PC). Run a release `redex.exe` on both, A in a
`steam:` world, B anywhere (offline is fine):

1. Each sees the other in the Friends tab, first, with a green dot and their status; a friend not running RE:DEX is
   greyed.
2. Chat both ways: the toast, the unread count on the tab, a tab per friend in the chat window; text arrives cleaned.
3. A invites B: B gets the prompt; Join lands B in A's world ("Joined steam:..."). Not now on another invite: A sees
   "B can't come right now".
4. B ignores A: A's chat and invites no longer arrive; restart B: still ignored (`<data>/user/ignored-<B's id>.txt`).
   Stop ignoring brings them back.
5. A's world has an allow list without B: B's Join shows "You're not on this world's list; ask its host" and B plays on.
6. Quit B's redex: within about 10 s A's list shows B as not in RE:DEX and A's chat box says "B isn't in RE:DEX".

If chat or invites never arrive while the world connection works, Steam networking messages between two app-480
clients don't deliver here; the fallback is to carry chat and invites through the world server (friends on the same
world only).

## Testing

`crates/xr-runtime/tests/two_instances.rs` runs the server and two headless games held to real time (`--realtime`)
with the `scripts/playthrough/mp-pair-*.txt` scripts:

| Test | What it checks |
|---|---|
| `vaniville` | A (the 100% save) flies to Vaniville; B (a new-game trainer saved in Vaniville) walks a loop by the door. A's puppet of B follows the loop (its place in the pool at 40 checkpoints, and exactly where xr-net put it); B walks off to Route 1 and the puppet goes within 2 s; B comes back, A saves with the puppet in the room (the move-model save equals one made without it), and the save reloads offline with the map's own NPCs only |
| `lumiose` | the same in Lumiose City (map 67), B a second copy of the 100% save that flies away and back |
| `vaniville_jitter` | `vaniville` with the server delaying every send 50-150 ms and dropping 10% of positions: no jump, no step back faster than walking |
| `server_restart` | the server is killed with both players on the map and started again 3 s later: both puppets go within 2 s and come back; no frame stalls |

```
cargo build --release -p redex-world --features sim          # sim brings mp-tests (--dev)
set XR_DATA=<a data dir whose build/x-usa-v0.2.0/xgame.dll has the hooks>
cargo test --release -p xr-runtime --features mp-tests --test two_instances -- --ignored --test-threads 1 --nocapture
```

The harness needs the `mp-tests` feature (`two_instances` requires it): `--dev-id` and a headless, scripted game in a
world exist only there. The same goes for exec.rs's world tests (`a_world_peer_becomes_a_puppet`,
`world_server_sees_the_walk`, `a_character_made_on_x_shows_on_y_after_login`). `make-release` builds without it.

### X and Y together

Pokémon X and Y players share the world, so the pair also runs as X + Y (scripts `mp-xy-*.txt`, driven by hand with
`XR_SET_FASHION` and the headless hooks below, now asserted by the tests named at the end of this section; the 100% save also boots in Y's folder, so Y can ride the bike):

- B (the biker) has a non-default outfit (`XR_SET_FASHION=<16 bytes>@1300`, written once the field is up, read when the
  Fly builds the player), flies to Vaniville, opens the item wheel and rides about; A (the other version) watches B's
  puppet: dressed in B's outfit, running in place at the bike's speed (the fallback; the bicycle itself is not drawn),
  with `motion` lines showing 4.5 world units a frame against 2.25 for running.
- `XR_MP_WHISPER=FRAME:TEXT` (B) whispers to the lowest session on the map once another player is on the map, and A's log shows
  `whisper from session N (name): text`. `XR_MP_REPORT=FRAME:REASON:NOTE` (A) reports that player (REASON 0 other,
  1 name, 2 chat, 3 cheat). A headless game has no menu, so these hooks stand in for the Whisper and Report panels
  (their requests are the same `Client::whisper` and `Client::report`). The server's `reports.jsonl` then has the line,
  with `target_steam_id` set for a Steam login and 0 for a dev id, and B's whisper in `target_whispers`.
- Both directions are tests in `two_instances.rs`: `xy_a_x_b_y` (A = X, B = Y) and `xy_a_y_b_x` (A = Y, B = X), `--ignored`
  like the others; they need the Y game too (`XR_CCI_Y`, `XR_DATA_Y`, `XR_SAVE_Y`, see the test file) and skip without it. They
  assert the outfit, the bike speed, the whisper and the `reports.jsonl` line.

- **Made characters.** `xy_created_looks` (A = X, B = Y) and `xy_created_looks_swapped`
  (A = Y, B = X): each player is a character made as the creator makes one (`characters.json`, the 100% save in the
  character's folder, Y's in Y's title folder) started with `--character`, two boys whose skin, hair and eyes differ
  from the save's and from each other's. They assert that each game applied its own look, that each sees the other's
  look on the puppet (`dressed <fashion>`), that A's change of clothes mid-meeting (`XR_SET_FASHION=<the save trainer's clothes in
  A's look>@3500`, standing in for a boutique) is kept as A's look and reaches B's puppet, and that the server's
  `accounts/` holds each dev id's character, A's in its new clothes.
- **Girls.** `xy_created_girls`: an X girl and a Y girl, new games in the bedroom, each sees the other dressed on the
  player set. `xy_created_girls_and_a_third` adds a third girl beside A (`XR_FAKE_PEERS`, which
  online is added to the world's players): A sees both dressed (B on the player set, the third on the NPC set) and
  leaves no girl out (`A's view: a/logs/top-cp-seen.png`).

`XR_MP_OUT` keeps the data dirs (screens, `logs/pair.log`) instead of the temp dir.

Scripts for networked runs wait on the other instance, never on frame numbers: `FRAME wait_until COND [TIMEOUT]` stops
the script clock until `puppets>=1`, `map!=0`, a memory word (`818290C=0`) or the screens' hash holds.

## Known limits

- A Delete deferred because a dressed puppet's parts are gone has no cap; the frozen puppet is not drawn and is freed at the
  next map change. The save sweep deletes a dressed puppet at once in the seconds after a battle.
- A join made while the game is paused resolves only on resume.
- A whisper reply goes by session id and goes stale after a same-address restart of a dev TCP server.
- Names show over a door's iris fade for a fraction of a second.
- No real controller has driven the panels yet (verified through the same entry point by tests); a manual pass is owed.
- A dressed puppet costs about 134 KB of model heap behind a 160 KB gate, so crowded maps get plain puppets (3 dress-up slots at most).
- The twin case (a puppet in the local player's exact outfit) is proved across a map change only.
- One girl outfit at a time on the player set (the player heaps); other girls wear theirs on the NPC set, where long
  hair is a plank and short hair is missing. Removing that needs the NPC set to draw her hair, or a third heap.
- A removed character's copy stays on the server's account (no delete message yet) and still counts toward its 8.
- A girl puppet can hold the game's second player heap. The game's own change of clothes in place, Mom's mirror in the
  intro (pyjamas to the starting outfit), went through with one on screen holding it (no fault, the player heaps
  unchanged). A boutique's fitting room cannot meet it (a girl in the Laverre boutique, zone 206):
  the game unloads DllField before DllKisekae loads, the field's dress-up loader goes with it (`[0x5E9EE4]` reads 0
  while the field is closed, as in the party menu), and DllKisekae makes its own with both heaps free; the change went
  through and the field came back in the new hat. A dressed girl holding the second heap through a field close (the
  party menu) is asked for again after it, no fault. In that boutique a girl puppet is never dressed anyway (its model
  heap has 343,800 bytes, under the 368,640 gate). The Lumiose salon (Coiffure Clips) has not been run.
- Surfers are hidden; bikers and skaters run in place at their own speed, with no bicycle or skating pose.
- In the pair tests the Y biker uses the X 100% save, and the fake feed's look rotation changes multi-peer looks.
- Trade, Battle and Invite to team come in M3, M4 and M7. Rich presence says "In Kalos" with no map name.
- A Refresh pressed while the server list is still being asked leaks one Steam request handle (a documented `shortcut:`).

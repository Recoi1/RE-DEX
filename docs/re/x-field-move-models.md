# X's field move models (x-usa-v0.2.0)

Plan 4 Task 1: the reverse engineering that the multiplayer puppets (Task 4) build on; plan 5 Task 1 added the
appearance, the movement modes and the camera (sections at the end). Read accessors:
`crates/xr-net/src/x/field.rs`. Test: `field_accessors_follow_a_walk` in `crates/xr-runtime/tests/exec.rs`, with
the script `scripts/playthrough/mp-field-walk.txt`.

## How addresses are written here

- **DllField+0xNNNNN** is an offset into DllField's code segment (CRO segment 0, which is file offset 0x180). It is
  the same offset `functions/x-usa-v0.2.0/cro/DllField.txt` uses. At run time the address is the code segment's
  address plus the offset. In the runs below ldr:ro placed DllField at 0x006A5000, so its code started at
  0x006A5180.
- **DllField data+0xNNN** is an offset into DllField's data segment (CRO segment 2). Its address is `segments[2]` of
  the module in `System.cro.modules`; the loaded CRO header holds it too, at segment table (header+0xC8) entry 2.
  In these runs it was 0x081820F0 (CRO buffer 0x08044000).
- **DllField ro+0xNNNNN** is an offset into segment 1 (read-only data and vtables). In these runs it was at
  0x00793000.
- **code.bin 0xNNNNNN** is an absolute address. code.bin is never relocated (text starts at 0x00100000), so these
  addresses are stable.
- **Heap addresses** (0x08xxxxxx) were stable from run to run because X's heap is deterministic. They are given
  only as examples from those runs. Code must reach them through the pointer paths below.

## Tools used (and added)

- `XR_WATCH=ADDR[+LEN],...` (new, `crates/xr-runtime/src/interp.rs`) logs every **interpreted** instruction that
  changes a watched word. Each line has the pc, lr, r0 to r3, and the words on the stack that look like code
  addresses (a rough backtrace). `XR_WATCH_MAX` caps the number of lines.
- `XR_INTERP_MODULES=DllField,...` (new form) leaves only the named CROs to the interpreter. `=1` still means all of
  them. With DllField interpreted, the field runs at about 150 fps, so the module's own writes can be watched.
- `XR_INTERP_ALL=1` interprets code.bin too. Booting the 100% save to the street takes about 15 minutes that way.
  That run is what found the code.bin writers.
- `XR_PEEK` and `XR_MEMDUMP` (an existing tool, from base 0x00300000 and 0x08300000) were run at the checkpoints of
  scripted walks from the 100% save: Pokemon Center (map 202), out of the door (map 200), four turns on the spot,
  a walk, a run and roller skates.
- Static analysis: llvm-objdump (from llvm-mingw) on DllField's code segment, taken from a memory dump so that its
  import stubs are already patched, and on exefs code.bin. A DllField import stub is
  `ldr pc, [pc, #-4]; .word target`, so each stub resolves to a code.bin address.

## The player

### Player snapshot: DllField data+0x7E4

DllField+0x2EB90 rewrites this struct every field frame. It is the old "position at 0x0818290C", which is this
struct's +0x38 while the data segment is at 0x081820F0.

| Offset | Type | Meaning | Evidence |
|---|---|---|---|
| +0x00 | ptr | a field-system object (also at player object +0x04) | dump |
| +0x10 | ptr | the player field object (0x1C8 bytes, below) | watch: the writer copies it from here |
| +0x14 | ptr | the **player controller** (below) | dumps, peek |
| +0x18 | u16 | **map (zone) id**: 202 in the Pokemon Center, 200 outside. It equals the save's Situation map id (save 0x1402 holds 202 in the 100% save, which was saved inside) | peek at 3 checkpoints, test |
| +0x1A | u8 | step phase (copied from controller +0x15) | peek |
| +0x1B | u8 | 1 while moving (copied from controller +0x14) | peek |
| +0x1C | u16 | **facing**: 0 up, 1 down, 2 left, 3 right. code.bin 0x4A3FA0 computes it from the move model. The save's Situation rotation was 3 and the first read was 3 | peek at 4 turns, test |
| +0x34 | u32 | 9 standing, 3 moving | peek |
| +0x38 | f32 x3 | **world position** x, y, z. One tile is 18 units; standing on a tile, x and z = tile * 18 + 9 | watch, peek, test |
| +0x48 | u32 | packed flags: 0x3000 standing, 0x1000 moving, 0x30xx on skates | peek |

Writer: watch on +0x38 (`XR_INTERP_MODULES=DllField XR_WATCH=818290C+C`) gives pc DllField+0x2EC20, inside
DllField+0x2EB90..0x2EF74: `ldr r0,[r4,#0x10]; ldr r1,[r0,#0x130]; str r1,[r4,#0x38]`, the same for +0x3C and +0x40.

### Player field object (snapshot+0x10, 0x1C8 bytes)

DllField+0x851D0 (a sequence step, state 2) allocates it (code.bin operator new 0x122920, size 0x1C8). Its
constructor is DllField+0xA59E0(this, ctx, param). It is rebuilt at every map change, so its address changes.

| Offset | Type | Meaning | Evidence |
|---|---|---|---|
| +0xE0 | u16 | zone id: the constructor copies `[[ctx+0x1C]+0x28]+0x0C` | watch: pc DllField+0xA5AAC |
| +0x130 | f32 x3 | world position (the snapshot's source) | watch, dump |

### Player controller (snapshot+0x14)

Its vtable is DllField ro+0x173C4 on foot and DllField ro+0x1A854 on skates. The object is rebuilt in place.

| Offset | Type | Meaning | Evidence |
|---|---|---|---|
| +0x08 | ptr | the **MoveModelManager** | dump; the same value as the manager found from the pool |
| +0x0C | ptr | the **player's move model** (a pool slot) | DllField+0xA9920 stores the result of the create call here |
| +0x14 | u8 | 1 while moving | peek |
| +0x15 | u8 | step phase | peek |
| +0x20 | u32 | **0x20 while running**, 0 when walking or on skates | peek: 4 run samples, 4 walk samples, 2 skate samples |
| +0x24 | u8 | **form**: 0 on foot, 2 on roller skates, and it stays 2 after skating stops | peek |
| +0x38 | f32 | speed per frame: 2.571 walking (18/7), 4.5 running (18/4), 4.35 to 6.48 on skates | peek |

`Mode` in field.rs: Skates if +0x24 == 2, Bike if +0x24 == 1, else Run if +0x20 & 0x20, else Walk. The bike was
found in plan 5 Task 1 (see "Movement modes" below): the same controller class, form 1.

### Appearance

See "Appearance (plan 5 Task 1)" below: gender and fashion are read live through GameData, and a puppet can be
dressed with any outfit.

## The move-model pool

| What | Where | Evidence |
|---|---|---|
| Manager (MoveModelManager) | controller +0x08 (0x08D01F5C in these runs) | dump: manager +0x1490 holds the pool's address |
| Manager constructor | code.bin 0x112CD0(this, heap, ?, count) | static: `__aeabi_vec_ctor_cookie` with code.bin 0x11EED8 and size 0x155 * 8 |
| Pool array | `[manager+0x1490]`; an 8-byte cookie {0xAA8, count} sits before it | dump, static |
| Slot count | u16 `[manager+0x1494]` = **32** | dump, static, accessor check |
| Slot size | **0xAA8** | dump (32 consecutive objects), cookie |
| MoveModel constructor and vtable | code.bin 0x11EED8, vtable **code.bin 0x5996A8** | static (the only literal reference to it) |
| Slot in use | status u32 at **+0x04, bit 0** | static (every pool loop tests it), dumps |
| Free slot | status +0x04 = 0, status2 +0x08 = 0x1800 (the reset state) | dumps, watch on a delete |

In use: 9 of 32 slots in the Pokemon Center (8 NPCs and the player) and 11 of 32 on the street (10 NPCs and the
player). Each slot holds a full move model, so free slots are not compacted. The game takes the first free slot.

### Move model fields (code.bin class, 0xAA8 bytes)

| Offset | Type | Meaning | Evidence |
|---|---|---|---|
| +0x00 | ptr | vtable, code.bin 0x5996A8 | dump |
| +0x04 | u32 | status. bit0 in use; bit1 active (set when setup ends, code.bin 0x3D78B4; cleared on delete); 0x40 and 0x400 are set by DllField for the player; 0x10000 init (code.bin 0x3D4EF0); 0x08000000 = came from the zone NPC list (code.bin 0x3D6EB4); 0x40000 = survives a DeleteAll that keeps persistent models | watch, static |
| +0x08 | u32 | status2. **bit 0x100000 = not saved** (below); 0x20000000 on skates; 0x4000000 is set by SetTransform when the position changed | static, peek |
| +0x0C | u16 | object id (from zone NPC entry +0x00; the player is 0xFF) | dump, static (Create) |
| +0x0E | u16 | zone id | dump, static (Create) |
| +0x10 | u16 | model id | dump (it matches the save entries) |
| +0x14, +0x18 | u16 | from zone NPC entry +0x06 and +0x0A (the script id is at entry +0x0A in pk3DS's layout) | static (Create) |
| +0x1C, +0x24 | f32 | facing as a unit vector (x, z): right (1,0), up (0,-1), down (0,1), left (-1,0). +0x34 and +0x3C hold a copy | peek at 4 turns |
| +0x64 to +0x83 | 8 x f32 | grid position block (+0x68 x tile, +0x6C y, +0x70 z tile); +0x94/+0x98/+0x9C hold the current tile | dump |
| +0xE8 | 0x2C bytes | **transform**: +0xEC world position x, y, z; then rotation; +0x108 to +0x110 scale | static (SetTransform), dump |
| +0x114 | f32 x3 | world position as drawn and collided (synced from +0xEC) | dump |
| +0x8FE | u16 | facing index (0 up, 1 down, 2 left, 3 right) | peek |
| +0x96D | u8 | speed code while moving: 0x10 walk, 0x19 run, 0x28 skates, 0 standing (`moving` in field.rs) | peek: 4, 4 and 2 samples |
| +0x998 | 0x4C bytes | block copied whole into the save entry | static (Store) |

## Functions

### Add: `MoveModel* code.bin 0x3D41D8 (MoveModelManager* mgr, const ZoneNpc* hdr, u16 zone, u32 extra)`

This is the add function NPCs use, and the player's own move model is created through it too.

- **Arguments.** `hdr` is a 0x30-byte zone NPC entry (pk3DS `OWSEStructs` layout): +0x00 id, +0x02 model,
  +0x08 spawn flag, +0x0A script, +0x0C facing, +0x1C/+0x1E a reuse key pair, and the position further on. `zone`
  is the zone id. `extra` is a per-NPC word from the zone loader's second array; the player passes its own.
- **Steps:**
  1. Checks the event flag `hdr+0x08` (code.bin 0x140170). The assert after it is compiled out.
  2. If `hdr+0x1C != hdr+0x1E`, it looks for an in-use model with matching fields (+0x52/+0x54/+0x56) and reuses
     it, updating its id, zone and +0x14/+0x18.
  3. Otherwise it takes the **first slot whose status bit 0 is clear**, sets `+0x0E = zone` and `status |= 1`, then
     initialises from the header (code.bin 0x3D80A8(mm, hdr), 0x3D90F4(mm, hdr, extra)) and resets the motion
     fields.
  4. Returns the move model.
- **Pool full:** it calls code.bin 0x11E864, which loops forever calling a fatal handler. **The game hangs.** It
  would also write through a null pointer. Task 4 must count free slots itself before calling it (Review Focus 3).
- **Callers:**
  - the zone NPC loader, code.bin 0x3D6DF0 (call at 0x3D6E9C), which DllField reaches through its import stub
    DllField+0x4718;
  - the player builder DllField+0xA9718 (call at DllField+0xA991C, through stub DllField+0x4A48);
  - the reuse path.
- **Evidence:** `XR_INTERP_ALL` + `XR_WATCH` on three slots' status across the door:
  - status 0 to 1 at pc code.bin 0x3D4380 for the two NPC slots added outside (backtrace 0x3D6EA0, then the zone
    loader's caller 0x3B8B8C);
  - the same for the player's slot (backtrace DllField+0xA9920, DllField+0x1CAA0);
  - DllField+0xA9718 then sets 0x40 and 0x400 through code.bin 0x3D037C, seen by the watch at
    DllField+0xA98D4/+0xA98E0.

**Zone NPC loader**, code.bin 0x3D6DF0(mgr, entries, count, zone, ...):
- for each 0x30-byte entry it skips the entry if its spawn flag is set;
- it requires fewer than 26 loaded for the zone (assert);
- it calls Create, then sets status 0x08000000.

This is the path the game uses for NPCs. A puppet can call Create directly with a 0x30-byte header built in guest
memory.

### Move

- **code.bin 0x3D0748 SetTransform(mm, const Transform\* t).** `t` is 0x2C bytes: +0x00 u16, +0x04 position
  x, y, z, +0x10 rotation, +0x20/+0x24/+0x28 scale. It copies `t` to mm+0xE8..0x110 and sets status2 0x4000000 when
  the position changed. If the model is active, it re-syncs the drawn and collision position at +0x114 (code.bin
  0x3D2D44). DllField+0xA9718 calls it (stub DllField+0x1430) to place the player.
- **code.bin 0x3D07F8 SetPosition(mm, const FieldPos\*).** It writes mm+0xE8 and then syncs +0x114 the same way.
  (Task 4: its argument is not a vec3 but the field's position object, whose first word is a pointer; see Puppets.)
  DllField import stub DllField+0x2538. Static only: a **hypothesis** that this is the per-frame "set position"
  (eidos's `FLDMMDL_SetPos`).
- Walking NPCs are driven by their movement code (zone entry move permissions → +0x14/+0x18, and the per-frame
  update through the vtable). For puppets, eidos's method (set the position every frame and interpolate) only
  needs SetPosition and the facing. **The animation call is open.** mm+0x824 changes with the motion (2 standing,
  3 walking, 7 stopping), so it is a likely motion id.

### Delete

- **code.bin 0x3D9A9C (MoveModel\* mm).** It clears status bit 1, runs the model's virtual teardown, then resets
  the whole object (code.bin 0x12BA88), which leaves status = 0 and status2 = 0x1800. Watch: the player's slot
  status 0x10443 to 0 at pc code.bin 0x12BAF0, lr 0x3D9B98.
- **code.bin 0x3D47E0 DeleteAll(mgr, all).** For each slot in use, it skips models with status 0x40000 unless `all`
  is set, calls 0x3D9A9C, then frees the model resource (code.bin 0x3D585C). It is called at map change (code.bin
  0x3C8050, which is in the watch backtrace), at the start of the save load (0x3D6D40), and from 0x3B7E14 and
  0x3CA0BC.

### Status bits

code.bin 0x3D037C SetStatusBit(mm, mask) does `status |= mask`. The DllField stub is DllField+0x7F8.

## Saving: FieldMoveModelSave

- **Block.** Save offset 0x2000, size 0x2100 = 32 entries of 0x108 bytes. The class's methods are in code.bin, and
  its vtable is near code.bin 0x59A950:
  - 0x42EE08 clears all entries;
  - 0x42EE38 returns 0x2100;
  - 0x42EE40 **Load(save, i, mm, mgr)** fills a move model from entry i if the entry's flag bit 0 is set;
  - 0x42F034 **Store(save, i, mm)** writes entry i from a move model;
  - 0x42F1EC also clears all entries.
- **StoreAll.** code.bin 0x4AAE64(mgr, GameData\*\*): the save object is `*GameData + 0x130C`. It clears the save
  object, then, for each slot in use **whose status2 bit 0x100000 is clear**, calls Store(save, n++, mm). The
  entries are compacted: the save holds the in-use, saveable models in slot order (the 100% save holds 9: 8 NPCs
  and the player).
- **What triggers a save.** code.bin 0x433B20(fieldctx):
  1. writes the save time;
  2. calls StoreAll(`[fieldctx+0x20]` = the manager);
  3. stores the rest of the field state (code.bin 0x4A1F88, ...).

  It is called from code.bin 0x1D3B98 and 0x1D4060 (the save flows; which UI path is which is open). Task 4's
  sweep has to run before 0x433B20.
- **LoadAll.** Around code.bin 0x3D6D3x:
  1. DeleteAll (0x3D6D40);
  2. for each free slot, while save entry n exists (code.bin 0x4AED24), Load(save, n, mm, mgr);
  3. it ends by clearing the save object (tail call 0x42EE08).

  Watch: the player's slot was written by Load (pc 0x42EE68, return 0x3D6DCC) at boot.
- **Entry layout**, from Store; `entry+X ← mm+Y`:

  | Entry | Move model |
  |---|---|
  | +0x00 | status (u32) |
  | +0x04..+0x12 | +0x0C..+0x1A (8 x u16: id, zone, model, ...) |
  | +0x14 | +0x1C (facing vector) |
  | +0x20 | +0x28 |
  | +0x2C/+0x2E/+0x30 | +0x4C/+0x4E/+0x50 |
  | +0x32/+0x33 | +0x58/+0x59 |
  | +0x34..+0x3A | +0x5A..+0x60 |
  | +0x3C | +0x64 (8 floats, grid position) |
  | +0x5C | +0x84 |
  | +0x68 | +0xBC (8 floats) |
  | +0x88 | +0xDC |
  | +0x94 | +0x148 |
  | +0x99..+0x9B | +0x150..+0x152 |
  | +0x9C | +0x998 (0x4C bytes) |
  | +0xE8 | +0x8C4 (16 bytes) |
  | +0xF8 | +0x8E4 (12 bytes) |
  | +0x104 | +0x8F0 |

- **Consequence for puppets (Review Focus 1).** A puppet whose status2 has bit 0x100000 set is never written to
  the save by the game itself. This is a **hypothesis from the code**: no save was made with such a model. Keep
  eidos's sweep before 0x433B20 anyway, and check the saved block against a lone save.

## Hit checks and talk (Review Focus 2): found in Task 4

- **Status bit 0x100 = no hit.** The pool search code.bin 0x4AA98C(mgr, &index, flags) returns the next model in use;
  flag 2 skips models with status 0x100, flag 4 skips models with status 0x400 (the player has 0x400). The hit
  checks code.bin 0x3D2534 and 0x4A6A84 search with flag 2, skip themselves and models whose status 2 has 0x4000000,
  then test the cylinder with code.bin 0x3D0B60. A puppet with 0x100 (set through SetStatusBit 0x3D037C) is walked
  through and blocks nobody. Test: `puppets_do_not_block_or_talk` (the player walks left through a puppet standing on
  the next tile and ends where it ends alone, x 8577).
- **Script 2000 = no script.** The zone data's hidden entries carry it, and code.bin 0x3D5A44 writes it into a slot it
  has just emptied. A puppet made with it: facing it and pressing A shows nothing and the walk after it is not held.
  The talk check itself (in DllField) was not traced.

## Puppets (plan 4 Task 4)

Code: `crates/xr-net/src/x/puppet.rs` (what to call), `crates/xr-runtime/src/hooks.rs` (where and how).

### Where game code is called from

- **Hooks.** `functions/x-usa-v0.2.0/hooks.txt` lists code.bin functions that generated code calls through
  `xr_rt->hooks[i]`. Only code.bin's own call sites go through them (CRO code calls code.bin through import stubs):
  - 0x15F860: the game's main loop, code.bin 0x1066D8 (called from 0x106284; the loop head is 0x1066FC), calls it once
    per pass (BL at 0x106730) before any proc runs. The puppet tick runs after it. Found from the main thread's stack
    while it waited on its frame arbiter (XR_THREADS).
  - 0x433B20: the field save. The puppets are deleted before it.
  - 0x3D41D8: Create, from code.bin's three call sites (the zone NPC loader's 0x3D6E9C among them). With no free slot,
    puppets are deleted until one is free, so the game never hangs in Create because of one.
  - 0x3D6DF0: the zone NPC loader, from code.bin 0x3B8B88 (the map change). Puppets appear only AFTER_LOAD (15) ticks
    after it ran for the player's map, so they allocate after the zone's own NPCs; a map whose load was not seen
    (DllField's own call, below) waits SETTLE_NO_LOADER (150) ticks instead.
  - Hooks fire only from native call sites in code.bin. A call from interpreted code, and every call from a CRO
    (through its import stubs), reaches the function itself. **DllField and DllUSR13 import Create and DeleteModel**:
    their calls skip the hook; MIN_FREE (6 free slots) is what protects them.
- **Threads.** The per-frame hook and the save run on the game's main thread ('main' in the kernel; the log says so
  at the first tick and at every save, with the save's caller). Both the menu's SAVE and the PSS's save before
  connecting came through the caller at code.bin 0x1D4060 (return 0x1D4064); 0x1D3B98 was not seen. The save hook only `try_lock`s the puppets: if it ever
  could not, the NO_SAVE bit alone keeps them out.
- **Calls.** A hook runs on the guest thread that made the call, with the baton. `GuestCalls::call` puts the arguments
  in r0-r3, sp 0x100 below the hook's sp (the 0x40 bytes there are the scratch area for arguments passed by pointer),
  lr = the hook's own `ret_lr` (taken at the hook's entry, before the original runs), and runs the function through
  `interp::interp_entry` (native when the DLL has it). The function being called is kept in an atomic; the main loop
  logs it if the per-frame hook stops for 300 frames while puppets are on (a game call that never returns).
  Every register, the flags and VFP are restored afterwards. A call can end the frame (the baton goes to the main loop
  and comes back), so the main loop only `try_lock`s the puppets.

### The calls

| What | Address | Notes |
|---|---|---|
| Create | code.bin 0x3D41D8(mgr, hdr, zone, 0) | a 0x30-byte zone NPC header (below) in the scratch area |
| Delete | code.bin 0x3D5818(mgr, mm) | Delete 0x3D9A9C, then the resource release 0x3D585C (as DeleteAll does per model) |
| Move | code.bin 0x3D0748 SetTransform(mm, t) | the model's own 0x2C bytes at +0xE8 with the position (+4) replaced |
| Facing | code.bin 0x3D33D4 SetDir(mm, dir) | 0 up, 1 down, 2 left, 3 right |
| Motion | code.bin 0x3D6FF0 SetMotion(mm, code, dir) | 0 stand, 12 walk, 13 run: the codes the player's step (DllField+0x705F4) uses; dir < 8 |
| No hit | code.bin 0x3D037C SetStatusBit(mm, 0x100) | |
| Not saved | status 2 \|= 0x100000 | no setter in the game: a plain write, checked again every tick (belt and braces; no rewrite was seen) |

**Correction:** code.bin 0x3D07F8 does not take a vec3. Its argument is the field's position object (built by the
import 0x19AA44 from `[[mm+0x898]+0x7C]`), whose first word is a pointer: a vec3 there faulted (read at the x
coordinate's bits). SetTransform takes a plain transform whose first word is 0 in every model seen.

A model is moved only once it is set up (status bit 0x2, set by code.bin 0x3D7800 on its first update): before that
the transform's first word is not yet 0.

### The header

The zone data's 0x30-byte entries (the zone loader's, 0x08CEC5F4 in these runs): +0x00 id, +0x02 model, +0x04 movement
code, +0x06, +0x08 event flag (Create ignores it), +0x0A script, +0x0C facing, +0x18/+0x1A 1 or 2, +0x1C/+0x1E reuse key
(equal: no reuse), +0x20 0xFFFF, +0x24 u32 0 (grid placement), +0x28 u16 tile x, +0x2A u16 tile z, +0x2C f32 height.
A puppet's: id 0x8000 and up, the player model, movement 0, script 2000, the remote's facing and tile, 1/1, 0xFFFF x3.

### Models

- DllField+0x1C8EC picks the player's model: **2 for a boy, 1 for a girl.**
- **A model that is not loaded hangs Create.** code.bin 0x3D4E78 (called by Create's init) takes the model's resource
  from the manager's table (u32 count at mgr+0x14BC, 0x18-byte entries from mgr+0x14C0, u16 model first) or from a
  model in use with the same id; otherwise, unless the byte at mgr+0x198C allows loading, it ends in the fatal loop
  0x11E864. Seen: a girl puppet (model 1) on a boy's map stopped the game's main thread inside Create. So a puppet's
  model must be resident; the other gender's falls back to the local player's model. Outside the Laverre Pokemon
  Center the table held 20 models (2, 50, 41, 26, 3, 5, 71, ...), not 1.
- The puppet drawn with model 2 wears the default look (red cap), not the local player's outfit.

### How many (Review Focus 3)

- **The heap runs out before the pool.** 30 fake players in the Laverre Pokemon Center (9 slots used) gave 17 puppets
  with 6 slots still free, and the game froze: the main thread sat in the fatal loop, from a model's draw setup
  (code.bin 0x3D7F70, `[mm+0xA10]` vtable +0x10) allocating through code.bin 0x12B0C8 (`heap` vtable +0x58) at
  0x3975C4. So puppets are capped at **8** (`MAX_PUPPETS`), and 6 slots stay free for the game. 30 players in the
  Pokemon Center, outside it and in Lumiose (map 67, after Fly) show 8 and the game runs on.
- **What one puppet costs**, from memory dumps at the same checkpoint with 0, 1 and 8 puppets (Pokemon Center):
  - app heap (0x08000000): one 756-byte block each (the model's work area), taken from a heap with 58 KB free;
  - linear (GPU-side) heap (0x14000000): about **71 KB** in about 194 blocks each (its model instance, skeleton and
    animation). The heap that holds them (free blocks at 0x14D2A744 and 0x14DF4DF4 in these runs) had 1,404,440
    bytes free with no puppet and 838,808 with 8, so 565,632 bytes for 8. 16 fitted in that map; the 17th
    allocation failed (largest free block, other allocations of those frames).
  - The cap of 8 leaves about 0.8 MB of that heap to the game in the Pokemon Center. Denser maps have less; the free
    size was not read live (the heap's own size method was not identified), so 8 is a measured cap, not a computed
    one.
- **Raising the cap later** (not built): share one model instance between puppets of the same look (most of the
  71 KB is per instance); lighter puppets (no shadow, a cheaper animation set); read the heap's free size and stop at
  a margin; draw distant players as a renderer overlay instead of move models.
- **Which 8:** with more players on the map, the 8 nearest to the local player (x/z) are shown, re-picked every tick.
  A shown puppet counts 3 tiles (`KEEP_MARGIN`) nearer than it is, so players at about the same distance do not swap
  in and out (each swap is a Delete and a Create). `Target.rank` goes first (0 for everyone now; friends and team
  members, M6/M7).
- After Fly the puppets came back before the new map's NPCs were loaded (3 slots used at the first spawn). The Create
  hook covers that order: with no free slot a puppet is deleted before the game's Create runs.

### Gender

From the save (MyStatus 0x14000 + 0x05) at boot (`xr_net::x::gender`), sent as the first appearance byte. A live path
to MyStatus is still not known.

### Saving (Review Focus 1)

- StoreAll 0x4AAE64 has one caller, the save 0x433B20 (`StoreAll([fieldctx+0x20], fieldctx)`, the block at
  `*fieldctx + 0x130C`).
- With XR_PUPPET_SAVECHECK=1 the save hook runs StoreAll with the puppets in the pool, then deletes them and runs it
  again: the two 0x2100-byte blocks are equal (`a_save_with_puppets_reloads_clean`). The save file holds the same 9
  entries as the original's room, no id from 0x8000, and loads with 9/32 slots in use.
- Negative control (a build with neither the bit nor the sweep): the save held 0x8000 and 0x8001, and the reload
  showed them as two stray NPCs (11/32 slots).
- Two separate runs, one with puppets and one without, save different bytes for the wandering NPCs (their facing,
  grid position and +0x8C4 state): the extra guest work shifts the game's timing. Their ids and count are the same.

### Fix round 1 findings

- **The field disappears for a few ticks**, and puppets must survive it:
  - in a wild battle DllField unloads and comes back with the same pool, the puppets still in it;
  - in some scripted events the player snapshot is all zeros (map 0, position 0,0,0) while the player stands
    elsewhere (Route 2's catching lesson, the Pokemon League lobby). `field::player` now refuses that snapshot.

  The first fix forgot the puppets whenever the field was missing and reaped them as orphans when it came back. In
  the League that meant a delete-and-create of all 8 every few seconds. Now the puppets stay known while the field
  is missing, and each slot is checked again when it returns:
  - a slot the game emptied or reused is forgotten;
  - an in-use slot with a puppet id (0x8000 to 0xFFFE) that xr-net does not know is deleted (an orphan);
  - a new move-model manager is a new pool, so the old one is never touched.
- **Orphans in practice.** In wild battles with the first version (puppets forgotten across a battle), 8 orphans were
  left in the pool after each battle and reaped. With the fix, a battle leaves the same 8 known puppets
  (`puppets_survive_battles_without_orphans`).
- **Heap, worst case** (30 fake players, so 8 puppets in each map; free bytes in model heaps 0x14B+0x14C, then the
  largest block):

  | Map | Free | Largest block |
  |---|---|---|
  | Lumiose 67 | 1,127,232 | 574,572 |
  | Coumarine 188 | 919,020 | 533,132 |
  | Anistar 211 | 804,800 | 508,524 |
  | Shalour 172 | 799,632 | 492,044 |
  | Snowbelle 222 | 817,584 | 508,524 |
  | Vaniville 0 | 955,404 | 539,084 |
  | League 243 | 1,167,388 | 584,516 |
  | League 245 (lobby to the Elite Four hall) | 1,086,520 | 566,316 |
  | Aquacorde 8 | 926,268 | 533,132 |
  | Route 2 259 | 1,127,164 | 574,560 |

  The lowest is 0.80 MB free with 8 puppets (Shalour). Each heap on its own (final review fix, 2026-11-19; free,
  then the largest block, with 8 puppets):

  | Map | 0x14B | 0x14C |
  |---|---|---|
  | Lumiose 67 | 552,660 / 552,616 | 574,572 / 574,572 |
  | Coumarine 188 | 385,888 / 385,844 | 533,132 / 533,132 |
  | Anistar 211 | 296,276 / 296,232 | 508,524 / 508,524 |
  | Shalour 172 | 291,172 / 291,128 | 508,524 / 508,524 |
  | Snowbelle 222 | 309,060 / 309,016 | 508,524 / 508,524 |
  | Vaniville 0 | 416,272 / 362,628 | 539,068 / 522,604 |

  The puppets' blocks come from 0x14B (0x14C's largest block is the same as the summed table's largest above). A
  spawn now needs a free block of PUPPET_HEAP (80 KB) + HEAP_KEEP (128 KB) in each heap, not a sum of both. The heaps are found from the linear-memory root head at 0x14000038 by their gfl ids
  (`x::heap`); they were found on every map above.
- **Trainer sight.** Three puppets stood still on the three tiles north of Youngster Austin (Route 2, tile 459,443,
  facing index 0) from before the player arrived. The battle started at the same moment as without them: the
  screens at the checkpoint during the battle (frame 14440) hash the same. So no sight trigger was seen on a puppet;
  the sight code itself was not traced.
- **Cutscenes.** The new game's Route 2 run (Mom's Rhyhorn send-off, the Hiker, Serena's catching lesson, three
  battles) was replayed with three still puppets beside the player on each map. The player's position at every
  checkpoint and the number of battles were the same as alone. Through the send-off the puppets kept the place their
  header gave them, so no "all NPCs" command moved them. During the catching lesson the snapshot is all zeros, so the
  puppets were not observed there. Not traced.
- **Clamps.** A remote position further than 32 tiles from the local player (x or z) is pulled in to 32 tiles. Puppet
  ids wrap within 0x8000..0xFFFE. A delete is done only for a slot inside the manager's array, at a model's start,
  that still holds the puppet.
- **Swaps.** With more than 8 players, a nearest-8 swap happens at most once every 30 ticks (about a second).

### Fix round 2

- **Zone NPC ids in X's data.** The zone files (romfs a/0/1/2, a GARC of 361 LZ11 "ZO" files) were scanned. Section 1
  (offset at +0x08) starts with u32 length, u8 furniture count, u8 NPC count, u8 warps, u8 triggers, u32, then
  0x14-byte furniture entries and 0x30-byte NPC entries; zone 200's 14 NPCs match the live dump. Across 360 zones there
  are 2,524 NPC entries. **The highest id is 0x1C**, and no entry uses model 1 or 2 (the player models). 457 entries
  have script 2000.
- **Puppet marks.** A slot counts as a puppet (to delete it, or to reap it as an orphan) only if all of these hold:
  - it is in use and has the expected id;
  - the id is 0x8000 or more;
  - its model is 1 or 2;
  - its script is 2000.

  No zone NPC carries all of these, and the player (id 0xFF) doesn't either.
- **Loader age.** A hooked loader run counts for a map only if it came at most LOAD_LEAD (120) ticks before the map's
  first tick (checked as: age <= ticks on this map + 120). Going A (hooked load), then B, then back to A with no hooked
  load waits for the 150-tick fallback; A's old run doesn't count. The run isn't cleared at a map change, because at a
  door the loader runs while the snapshot is gone, and the first ticks after can still show the old map. A save
  restarts the map's tick count, so its load still counts only if the save came within 120 ticks of the load; after a
  longer stay the puppets come back after the 150-tick fallback.
- **make_room without a snapshot.** It uses the last manager the puppets lived in, if that is this Create's manager and
  `field::slots` still validates it.
- **A poisoned lock** (a panic while holding the puppets) is taken anyway and logged once (`hooks::try_puppets`).

## Motions (plan 4 Task 5)

**SetMotion (code.bin 0x3D6FF0) sets an action command**, not an animation: it calls DllField text+0x49428
(through the import stub code.bin 0x19AEBC) with the model's action object (mm+0x8F4), the code (below 0xD4) and a
direction from the byte table at code.bin 0x542F14 (`1 0 3 2 7 6 5 4`). So SetMotion's own directions are **0 down,
1 up, 2 left, 3 right**; Task 4 passed the facing (0 up, 1 down), which turned a puppet's back to the camera when it
faced down. The action state is the u32 at mm+0x8FC (1 playing, 2 ended); the u16 halves at mm+0x900 count the
updates left and the action's length.

**Walk (12) and run (13) move the model.** They are the player's one-tile steps: at every field update (every other
frame; the puppet tick runs every frame) the model goes 2.25 (walk) or 4.5 (run) from its own grid tile toward the
next, whatever SetTransform set. Found by the two-instance harness: on grid maps the drawn puppet jumped up to 4
tiles for a few frames when the two disagreed, and stood a tile and a half past its place after each stop; in
Lumiose City the step went the other way. The tick had also moved puppets at twice the player's speed (the speeds
were per field update).

**A scan of the codes** (a still puppet, each code for 60 frames, re-issued; drift = how far the model got from where
SetTransform put it; the screens checked by eye):
- 8 and 9 hang the game (the main thread stops in the field update); 50 hides the model.
- Codes that move the model: 10 to 22, 29 to 36, 66 to 69, 71 to 75, 77, 80 to 86, 89, 90, 92, 93, 143, 146, 149,
  151 to 153, 155 to 158, 202 to 204, 210.
- **25 to 28 step in place**: legs and arms as in a walk, the model does not move. They last 16, 8, 4 and 2 field
  updates (a walk step, 12, lasts 8, as does 26; 27 as long as a run step).

**Puppets now** move by SetTransform only (anything else that moved the model is put back each frame), at the
player's speed per frame (walk 18/14, run 18/8), and play 26 (walk in place) or 27 (run in place) while they move,
set again each time the action ends; they stand (0) after 8 frames without moving.

## Appearance (plan 5 Task 1)

Accessors: `field::appearance` / `Appearance` (`validate`, `to_bytes`, `from_bytes`) and `puppet::dress`. Tests:
`appearance_reads_through_gamedata`, `appearance_validates_like_the_game`, `dress_builds_the_players_header`, and
`field_appearance_bike_and_camera` in `crates/xr-runtime/tests/exec.rs` (script `scripts/playthrough/mp-field-modes.txt`).

### In the save

MyStatus is the save block at **0x14000**. The fields used here (PKHeX's names; the layout below is the game's own):

| Save | Size | Meaning |
|---|---|---|
| 0x14005 | u8 | gender: 0 boy, 1 girl |
| 0x14030 | 16 bytes | **trainer fashion**: four u32 bit fields (below) |
| 0x14048 | 0x1A | the trainer's name (UTF-16) |

Known saves:

| Save | Fashion |
|---|---|
| the 100% save (boy) | `88b91905 d0093203 11000000 00000000` |
| the new-game boy "Red" (first look, after Mom's mirror) | `48640901 01cb0803 41000000 ...` |
| the same boy in pyjamas, in the bedroom | `48040901 0a6a0000 41...` |
| a new-game girl (third look, bedroom; gender byte 1) | `c0002402 4c059002 00040000 ...` |
| the new-game girl in pyjamas, in the bedroom (measured with `--make-preview girl`'s chain, XR_PEEK-style appearance line) | `40042402 4c059002 00042000 00000000` |
| the new-game girl's starting outfit (after Mom's mirror, script frame 8600; **`DEFAULT_GIRL`**) | `403c2402 00201303 41052000 00000000` |

The intro's gender pick: the boy/girl question is up from script frame about 2750 in `new-game.txt`. A at 2775 picks the
boy (left mirror). `scripts/preview/girl.txt` adds `2765 RIGHT 4` first, which picks the girl (right mirror); the name
screen still comes up by 3150 and the keyboard taps at 3300-3400 land unchanged. Move model: 2 boy, 1 girl.

### In memory

- **Path:**
  - DllField data+0x7E4 (the snapshot) +0x08 is the field context: the object the save 0x433B20 takes. Its +0x20 is
    the move-model manager.
  - `[context+0]` is **GameData** (vtable code.bin 0x59AE00). GameData holds every save block inline, and a table of
    pointers to them at +0x61A70. Its constructor fills the table (code.bin 0x1164F4 on).
  - Entry 17 of that table, **GameData+0x61AB4**, points to the **MyStatus** object (vtable code.bin 0x59ADC0).
  - The save block's bytes start at the object's +0x08. So gender is at +0x0D and the fashion at +0x38.
- **Evidence:**
  - Memory dumps at three checkpoints: the Pokemon Center, outside, and a tile left.
  - The MyStatus bytes match the save, except the bytes that change at boot (+0x10..+0x2B).
  - The new-game girl's dump gives gender 1 and her fashion.
  - The same path held on every map tried: 202, 200 and 2.

### The intro's commit (start-screen plan 2 Task 8: no hook)

Found with `XR_INTERP_ALL=1 XR_WATCH` on MyStatus (object 0x08C79C24: +0x0C, +0x38, +0x50) from a save state at
`new-game.txt` frame 2700, once with the boy and once with `2765 RIGHT 4` (the girl). DllIntro is placed at 0x006A5000,
its code at +0x180; the offsets below are from its code segment, as in `functions/<id>/cro/DllIntro.txt`.

- **Look pick (about frame 2775), DllIntro code+0x6E84..0x6F84.** MyStatus = GameData+0x129A8. The intro's work holds
  the gender at +0x3D and the look (one of 3 presets) at +0x40. Through import thunks, it calls code.bin's setters:
  - gender (code.bin 0x436BE4, `+0x0D`);
  - the gender's default outfit (a memcpy from 0x5AE5A8 for the boy, 0x5AE5C8 for the girl);
  - the multiplayer sprite bytes +0x0E/+0x0F (0x4365D4; boy 01 01, girl 00 00);
  - three preset features from a table (gender * 9 + look * 3 halfwords): 0x4364C0, 0x436728, 0x436618.
  The intro has only those 3 presets per gender, so it can't show an arbitrary creator look anyway.
- **After the name entry (DllStrInput unloads, about frame 3500), DllIntro code+0x6FC0..0x8708, the intro's sequence:**
  - +0x8074 and +0x8080: the typed name (the work's +0x64) goes to MyStatus +0x50 and to a second copy at +0x6A
    (save 0x14062; likely the nickname, not checked), through code.bin's string copy 0x14E9FC.
  - +0x82FC: code.bin 0x43582C(GameData+0x1058, gender == girl) fills the gender's starting clothes: the owned-items
    bits at GameData+0x1058.. and six outfit slots of 0x10 at +0x10C4.. (the default outfit).
- Boy and girl memory at the `fletchling` checkpoint (GameData+0..+0x62000) differ only in MyStatus +0x0D..+0x0F, the
  outfit, and that clothes block. No event work records the gender: the rival comes from MyStatus at run time. Checked:
  a girl character over a boy intro (Task 7's pre-field write) meets **Calem and Shauna** outside the house
  (`art/ref/character-girl-over-boy-intro-rival-calem.png`).
- **Why no hook:** every commit is DllIntro (a native CRO) calling code.bin through its import thunks. xgame.dll's
  `xr_lookup` resolves those calls to the code.bin function itself, so neither `xr_rt->hooks` (code.bin call sites
  only) nor the runtime's `hook_for` (interpreted call sites only) sees them. No code.bin function commits the choices,
  and no native code.bin call site reaches the setters during the intro. A hook would need the module code generation
  to route import calls through the hook table, which means a rebuild of every player's xgame.dll.
- **Left to the pre-field write (Task 7):** it sets gender, outfit and the name at +0x50. It does not set the sprite
  bytes +0x0E/+0x0F, the name copy at +0x6A, or the clothes block at GameData+0x1058, so these stay the intro's
  gender's and name's.

### The game's ranges: code.bin 0x16A4F8(out, in, gender)

This function checks a fashion: 0x16A350 for a boy, 0x16A584 for a girl. If any field is out of range, it copies the
default outfit instead and returns 0. The bit fields (word, first bit, width) and the largest value it accepts are
below; `Appearance::validate` uses exactly these.

| Boy | Range | Girl | Range |
|---|---|---|---|
| w0 0..2 | must be 0 | w0 0..2 | must be 0 |
| w0 3..5 | **must be 1** | w0 3..5 | **must be 0** |
| w0 6..7 skin | not checked (1, 2 and 3 seen) | w0 6..7 skin | not checked |
| w0 8..10 hair colour | ≤ 4 | same | ≤ 4 |
| w0 11..15 hat | ≤ 23 | w0 11..16 hat | ≤ 31 |
| w0 16..18 | ≤ 1 | w0 17..19 | ≤ 2 |
| w0 19..22 hair | ≤ 4 | w0 20..23 hair | ≤ 6 |
| w0 23..25 face | ≤ 4 | w0 24..26 face | ≤ 4 |
| w0 26..27, 28..29 | ≤ 1 each | w0 27..28, 29..30 | ≤ 1 each |
| w1 0..5 top | ≤ 35 | w1 0..5 top | ≤ 35 |
| w1 6..10 bottom | ≤ 17 | w1 6..12 bottom | ≤ 45 |
| w1 11..13 socks | ≤ 5 | w1 13..16 dress | ≤ 10 |
| w1 14..18 shoes | ≤ 13 | w1 17..21 socks | ≤ 27 |
| w1 19..22 bag | ≤ 7 | w1 22..27 shoes | ≤ 27 |
| w1 23..26 hat accessory | ≤ 12 | w1 28..29 | ≤ 1 |
| w1 27..28 | ≤ 1 | w2 0..4 bag | ≤ 16 |
| w2 0 contacts | flag | w2 5..9 hat accessory | ≤ 24 |
| w2 1..3 | ≤ 2 | w2 10 contacts | flag |
| w2 4..6 eye colour | ≤ 4 | w2 11..12 | ≤ 2 |
| w2 7..9 | ≤ 4 | w2 13..14, 15..16, 17..18 | ≤ 1 each |
| w2 10..13, 14..17 | ≤ 7 each | w2 19..21 eye colour | ≤ 4 |
| w2 18..20, 21..23 | ≤ 4 each | w2 22..24, 25..27, 28..30 | ≤ 4 each |
| w2 24..25 | ≤ 1 | w3 0..2 | ≤ 4 |
| w2 26..28 | ≤ 4 | w3 3..4 | ≤ 1 |
| | | w3 5..7 | ≤ 4 |

Bits the game never reads are 0 in every save seen, and `validate` requires 0 there:
- a boy's w0 30..31, w1 29..31, w2 29..31 and all of w3;
- a girl's w0 31, w1 30..31, w2 31 and w3 8..31.

The game does not check skin, and the skin value goes straight into the dress-up body. `validate` therefore allows
only the values seen in saves, 1 to 3 (fix round 1). Widen it only after skin 0 has been tried live.

### How the field dresses the player

**The dress-up parameter** is 0x4C bytes at **move model +0x998**. This is also what the move-model save entry's
+0x9C holds.
- A 12-byte header:
  - u8 0;
  - u8 body layout: **1 boy, 0 girl**;
  - u8 0xFF;
  - u8 1 (the player's value; the NPC default path writes 0);
  - u16 0xFFFF, twice;
  - a pointer to the body. After the copy it points into the model itself (+0x9A4).
- A 0x40-byte **body**:
  - u16 skin, then u16 hair colour;
  - one s16 fashion item id per category (13 for a boy, 14 for a girl), -1 for none;
  - then makeup and colour fields.
  - the save trainer's body: `2, 1, 22, 23, 26, 29, 47, 73, 84, 96, 107, 109, 115, -1, -1, 0, 0, -1 ...`.

**The converter** is code.bin **0x12EA10(body, fashion)** for a boy and **0x12ED30(body, fashion)** for a girl.
- Each first runs the check above.
- The item id is `base[category] + value - 1`, or -1 when the value is 0. The boy's bases, at code.bin 0x542004:
  0, 23, 24, 28, 32, 67, 84, 89, 102, 109, 110, 122, 123.
- **Verified live:** 0x12EA10 called with the 100% save's fashion gives the player's own +0x998 body, byte for byte.

**Create's `extra` is the parameter** (its 4th argument, a pointer).
- Create's init, code.bin 0x3D90F4(mm, hdr, extra), runs this only when the model can be dressed. That is model byte
  **+0x88B**, set from the model's resource; it is 1 for models 1 and 2.
- With `extra` set, the init calls code.bin 0x46E1BC(mm+0x998, extra), which copies the header and the body.
- With `extra` 0, it builds a default parameter. This is why a model-2 puppet created with `extra` 0 wears the default
  look (red cap).
- The player builder DllField+0xA9718 does the same with its own parameter, through code.bin 0x46E1B0(mm, param).

**Drawing.**
- Move model **+0x87F** picks the draw handler, through code.bin 0x19AD6C (a DllField export): **1 for the player
  models**, 2 for NPCs. The handler is stored at +0xA10.
- The player's handler is at DllField ro+0x18384; its +0x10 is DllField+0xDA72C. If +0x88B is set, the handler
  builds the model through **DllField+0x63820(sys, param, ...)**.
- That call only succeeds when one of the model system's **20 dress-up part slots** holds this exact outfit, loaded.
  While no slot does, it returns 0 and the handler retries at every update, so the model is not drawn.

**The part slots.**
- The model system is **[manager+0x14B0]**; every move model's +0x8B0 points to it too.
- The slots start at sys+0x1DF8, 16 bytes each:
  - u8 request: 1 load, 4 unload;
  - u8 state: 2 loading, 3 loaded, 5 unloading;
  - the 12-byte parameter. Its body is a heap copy (code.bin 0x464480).
- DllField+0x783D0 runs the slots at every update. code.bin 0x4681C4 compares two parameters, header and body.
- **DllField+0x52428 RequestDressUp(sys, param):** if a slot already holds the outfit, it sets that slot's request to
  1. Otherwise it takes a free slot, copies the parameter in and sets request 1.
- **DllField+0x5250C ReleaseDressUp(sys, param):** sets the matching slot's request to 4 (unload), or to 0 if the slot
  was unloading. Run live since plan 5 Task 3 (below, "Dressed puppets").
- In the Pokemon Center, slot 0 held the player's outfit (state 3) and the other 19 were free.

### Proved live

The runs used a temporary patch to `Puppets::spawn`. It is not committed; it is kept as `dress-experiment.diff` in
that folder. In each run one puppet (model 2, XR_FAKE_PEERS=1) stood in the Laverre Pokemon Center.

| Create's `extra` | Puppet |
|---|---|
| 0 | the default look: red cap, brown hair (as in M1) |
| `dress()` with the local player's own fashion | **dressed as the player**: panda hat and sunglasses (`puppet-default-vs-local-outfit.png`). The parts were already loaded, in slot 0 |
| `dress()` with Red's fashion (another outfit), no request | **not drawn**: no slot holds the outfit, and the model heaps did not change in 640 frames |
| `dress()` with Red's fashion, after RequestDressUp(sys, param) | **dressed as Red**: blond hair, red cap with sunglasses, blue jacket (`puppet-default-vs-other-outfit-zoom.png`). Slot 1 took the request |

### Contract for Task 3 (applying an appearance to a puppet)

1. Call `Appearance::from_bytes(peer bytes)`. If it returns None, keep `extra` 0 (the plain boy or girl look).
2. Pick the model: `MODEL_GIRL` if gender is 1, else `MODEL_BOY`. M1's residency fallback still applies: an absent
   player model hangs Create.
3. Build the parameter with `puppet::dress(g, at, &a)`.
   - `at` is DRESS_SIZE (0x60) bytes of guest memory that stay valid through the next two calls.
   - The 0x30-byte zone header sits at scratch+0, so use scratch+0x30. The hook's gap is 0x100 bytes, and its
     callees' stacks grow down from the scratch address.
4. Request the parts: `sys = [manager + MGR_MODEL_SYS]`, then call DllField code + REQUEST_DRESS (0x52428) with
   (sys, param). The parameter is copied, so it need not outlive the call.
5. Call `Create(mgr, header, zone, param)`. The puppet appears once its slot reaches state 3.
6. On delete, or when the outfit changes, call DllField code + RELEASE_DRESS (0x5250C) with (sys, param) for the old
   outfit.
   - Release **only outfits that xr-net requested and that are not the local player's own**: releasing the player's
     slot would unload the player's own parts.
   - Slots are shared by outfit, so count the puppets per outfit.
   - There are 20 slots in all, and the player uses at least one.
7. Not measured:
   - the heap a slot's parts take. With Red's slot and its puppet, the 0x14B model heap had about 35 KB less free
     than with an undressed puppet;
   - how long a load takes. It had finished by the first checkpoint, 40 frames later.

### Dressed puppets (plan 5 Task 3)

Code: `Puppets::spawn` and `release_unworn` in `crates/xr-net/src/x/puppet.rs`.

- **RequestDressUp, disassembled** (DllField+0x52428): for each of the 20 slots it compares the slot's parameter with
  the given one (DllField+0x26B0). On a match it sets the slot's request to 1 if the state is 0 or 5, else to 0, so
  asking again for a loaded outfit does nothing. With no match it takes the first slot whose request and state are both
  0, copies the parameter (body included) in and sets request 1. With no free slot it does nothing.
- **ReleaseDressUp** (DllField+0x5250C): on a match, state 0 (asked for, not started) frees the slot at once (the body
  copy too); state 5 (unloading) sets request 0; any other state sets request 4 (unload).
- **The parameter compare** (code.bin 0x4681C4, used by Request, Release and the draw lookup): header bytes 0, 1, 2
  and 3, the u16 at +4 and +6 (+4 not -1: equal without the body), then the 0x40-byte body.
- **What xr-net does (fix round 1):**
  - a puppet's parameter has header byte 3 = 0 (`PUPPET_MARK`, the value the game's NPC default path writes; the
    player's is 1). So a puppet never shares a slot with the player, not even a twin in the player's exact outfit
    (live: 2 slots for one outfit, the twin drawn in it, `task3-shots/twin-pc-outside-gone.png`). The game's release
    of the player's old outfit cannot unload a puppet's parts, and xr-net's cannot unload the player's;
  - asks for an outfit only on the model of its own gender (a stand-in model gets the plain look);
  - needs more than DRESS_KEEP (2) free slots and PUPPET_HEAP + DRESS_HEAP (160 KB) + HEAP_KEEP in each model heap,
    else the puppet is made plain. **PUPPET_MARK costs heap:** in the Pokemon Center a dressed puppet took 73,024
    (0x14B) and 134,592 (0x14C) bytes in all with byte 3 = 0, against 34,896 and 6,048 with the player's byte 3 = 1
    (a plain puppet: 54,224 and 16,480). What byte 3 selects was not traced;
  - makes no dressed puppet while any slot is loading or unloading (request 1/4, state 1/2/5), so each outfit's
    parts are in the heap before the next heap check; after DRESS_WAIT (600) ticks of a busy loader the puppet comes
    plain, and the wait is logged;
  - releases by the slot's own state, RELEASE_AFTER (60) ticks after the last puppet wearing the outfit went: a slot
    still loading waits; a slot with a request xr-net did not make, or whose parameter one of the game's own models
    wears (+0x998), is left to the game; no slot at all: nothing to release;
  - remakes a puppet whose outfit's slot is unloading (request 4 / state 5) or gone.
- **Releasing in the same tick as the Delete hangs the game.** First live run: the puppet was deleted (outfit change)
  and its outfit released in the same tick; the main thread then sat in the heap's error loop (code.bin 0x12B2B4,
  calling its handler 0x10D4DC forever) and the screen froze. With the release 60 ticks after the Delete the same run
  went through all five phases. The deleted model's teardown evidently still uses the parts after DeleteModel returns.
- **The model system is per map.** At a door the manager's model system moved (0x082870D8 in the Pokemon Center,
  0x0828BDE0 outside), with only the player's slot in the new one. xr-net's list of asked-for outfits is keyed by the
  system and starts over. After a twin (its own slot) left on the new map, the pool, the dress-up slots and both
  model heaps equalled the same run without any puppet (694,960 / 632,108).
- **Across a wild battle (fix round 2).** Traced with a watch on a dressed puppet's draw binding and its slot
  (DllField interpreted, `mp-puppet-battle.txt`):
  1. At the field close the game takes **every** move model's draw binding down first: code.bin 0x3D6818-0x3D6854
     clear mm+0xA04, +0xA08 and +0xA10, the draw handler, for the puppets too.
  2. Then DllField+0x522AC (ReleaseAll, called from DllField+0x518EC) releases every dress-up slot in use, the
     player's and xr-net's, and runs the slot runner (DllField+0x783D0) in the same call, so the parts are freed at
     once. No puppet tick runs between 1 and 2, nor before 1 with a sign that a close is coming, so there is no
     pre-free point to delete the puppets from.
  3. DllField unloads; the pool and its models stay. The model system keeps its address; its slots are re-initialised
     when DllField is back.
  4. code.bin 0x3D4138-0x3D4160 binds the draw again for every model. A dressed puppet's draw then waits for a loaded
     slot with its outfit (DllField+0x63820), as after a spawn.

  So a dressed puppet holds no reference to its freed parts across a battle. xr-net never deletes it while its
  outfit's slot is gone or unloading: it asks for the outfit again (`ask_again`), and any Delete waits until a slot
  holds the outfit again. Every Delete logs the state of its outfit's slot (`; its outfit in slot N, request R state
  S`).
- **The local player changes clothes** (live: XR_SET_FASHION writes MyStatus, the game builds the player in it at the
  next door): the twin, in its own slot, is untouched; outside it appears again in its outfit beside the player in
  the new one.
- **Heap, Laverre Pokemon Center** (model heap 0x14B free bytes; 0x14C in brackets): no puppet 756,344 (648,140);
  Red's outfit on a puppet 721,448 (642,092); a plain puppet 702,120 (631,660). A dressed puppet with its outfit's
  parts took less than a plain one: the plain look loads the default outfit's parts for it.
- **Saving:** the dressed puppet's +0x998 holds its parameter (the save entry's +0x9C); puppets are swept before the
  save and carry status 2's NO_SAVE, so it never reaches the file (`a_save_with_puppets_reloads_clean` runs with
  two dressed bikers).

### The bicycle on a puppet (plan 5 Task 3, time-boxed: not solved)

How the player mounts, found by a watch on the player's mm+0xA24 (`XR_INTERP_MODULES=DllField`), during
`mp-field-modes.txt`'s ride. The mount code is at DllField+0xABB7C..0xABC30 (r4 = a ride-state object: +8 manager,
+0xC the player's move model, +0x2C the attachment entry). It calls:
1. code.bin 0x3D1E20(mm, 2): mm+0x59 = 2;
2. code.bin 0x3D09B8(mm, 0): mm+0x58 = 0 (through a callback table at code.bin 0x5AE698);
3. code.bin 0x3D9874(mm, 0x4C, facing): an action set directly (the facing from code.bin 0x4A3FA0);
4. DllField+0x8B2B4([manager+0x14B4], 0x10, mm): takes a free entry (0x15C bytes) in an attachment table and stores
   type 0x10's resource and the model; the index goes to the ride state's +0x2C. Other forms register other types:
   0x28 at DllField+0xAC040, 0x0B at DllField+0xAB3A8;
5. SetStatusBit 0x200000 and 0x1000000 (and 0x2000000 when code.bin 0x4AAF98(mm) says so);
6. code.bin 0x3D83CC(mm, 3): mm+0xA24 = 3 (1 on foot).

DllField's import stubs were resolved with XR_PEEK at run time (DllField at 0x006A5000): +0xF18 = 0x4A3FA0,
+0x1238 = 0x3D9874, +0x1068 = 0x4AAF98, +0x3BC8 = 0x3D1E20, +0x810 = 0x3D09B8, +0x4A58 = 0x3D83CC, +0x7F8 = 0x3D037C.

Tried on a dressed puppet (an experiment build, not committed), with motion 77: mm+0xA24 = 3; plus the status bits;
plus steps 1 and 2; plus the attachment entry (it returned entries 12 to 15). Indoors and outdoors (map 200), and
spawned after the player had mounted (so the bike's resource was loaded): **always the riding pose with no bicycle**
(`task3-shots/bike-*.png` in the plan 5 sdd folder). Still to look at: what fills and draws an attachment entry
(its 0x15C bytes after registration), and whether the bicycle is the player model's own mesh switched by the draw
handler. An attachment entry must also be released before its puppet is deleted, which was not traced.

**Shipped:** a puppet on the bike runs in place (motion 27) at the bike's speed, the ledger's fallback.

**Skates (fix round 1, time-boxed):** SetMotion(mm, 210, dir) on a skating puppet (the player's skating code) was
called every step (XR_PUPPET_TRACE shows it), but frames 1440-1488 of the run were pixel for
pixel the frames of the same run with the run-in-place motion (`task3-shots/skates-*.png`): without the skate form the
model shows no skating pose. Shipped: skating puppets run in place (27) at the skates' speed.

### Known limits (accepted)

- **PUPPET_MARK costs model heap:** about 134 KB of 0x14C per dressed puppet against 6 KB with the player's byte 3.
  With the 160 KB gate, crowded maps get plain puppets.
- **The twin path was proven across a map change only**, where the game rebuilds the player in the new outfit. A
  mid-map change (a boutique) was not run; with separate slots the game's release cannot reach a puppet's parts.

## Movement modes (plan 5 Task 1)

Measured from the 100% save outside the Laverre City Pokemon Center, with `mp-field-modes.txt` and a longer script
with memory dumps at each checkpoint. The bike is the item wheel's RIGHT slot: press Y, then RIGHT.

| Mode | Controller +0x24 form | Controller +0x20 | Speed per field update (+0x38) | mm +0x96D | mm +0x8FC u16 (action) | Status 2 | Snapshot +0x48 |
|---|---|---|---|---|---|---|---|
| Walk (standing) | 0 | 0 | 0 | 0 | 0 (stand) | 0x4402 | 0x3000 |
| Walk | 0 | 0 | 2.571 (18/7) | 0x10 | 0x12 | 0x4402 | 0x1000 |
| Run | 0 | 0x20 | 4.5 | 0x19 | 0x0D (13) | 0x4202 | 0x1000 |
| Bike, riding | **1** | 0 | **9.0** (2 updates a tile) | **0x38** | **0x4D (77)**; 0x4E (78) as it stops | **0x20000000** set | 0x3050 / 0x305C |
| Roller skates | **2** (stays 2 standing) | 0 | 4.35 to 6.48 | 0x28 (0x21 starting) | **0xD2 (210)** | 0x20000000 while skating | 0x30xx |

- **The same controller and model on the bike.** The controller class (vtable DllField ro+0x173C4) and the model (2)
  do not change. The bike is not another move model either: 11 slots were in use before and after.
- **One bike.** X has no Mach/Acro split: B while riding changed none of these values.
- **Getting off** (Y, then the bike again) sets the form back to 0.
- **A puppet on a bike:**
  - SetMotion 77 on a model-2 puppet gives the riding pose (leaning forward, hands out) but **no bike**
    (`puppet-motion-77.png`, `player-bike-vs-puppet-77.png`).
  - Setting status 2's 0x20000000 as well changed nothing visible.
  - The bike model comes with the player's form change, which was not traced (open). Until it is, show bike puppets
    with motion 77 or with the run-in-place motion.
- **Surf: not reached.**
  - The 100% save starts in Laverre City. After a Fly to Coumarine City, the walk south from its Pokemon Center met
    a fence, not water.
  - Surf's form value and motions are open.
  - The likely places to try: Coumarine's harbour (south of the lower town), or any route with water reachable after
    a Fly.

## The field camera (plan 5 Task 1)

Accessor: `field::camera(g, data) -> Option<ViewProj>`, then `ViewProj::project([x, y, z]) -> Option<[f32; 2]>`.
Tests: `camera_projects_onto_the_top_screen` (unit) and `field_appearance_bike_and_camera` (live).

- **Where:**
  - DllField data+0x7E4 (the snapshot) +0x10 is the player field object.
  - Its **+0xF0** is the camera block; the manager's +0x18 points to the same block.
  - The block's **+0xB94** holds a **4x4 f32 row-major view-projection** matrix: `clip = M * (x, y, z, 1)`.
- **Writer:** DllField+0x745FC stores the view there (a look-at built on the stack, code.bin 0x144EB4). Then
  DllField+0x7464C multiplies the projection in place (code.bin 0x122A68). This happens every field update, so a
  read after a frame gives that frame's camera.
- **Orientation:**
  - The matrix is the landscape view the player sees. Row 0 maps to the 400-pixel axis, row 1 to the 240-pixel axis
    (up), and row 3 is w (minus the view z).
  - The 90-degree turn for the 3DS's sideways framebuffer comes later, in the rotated matrices the renderer gets. In
    these runs they were at 0x08510238 and 0x085109F4, holding `[0, cot, ...; -cot/aspect, 0, ...]`.
  - Pixels at 1x: `px = (1 + x/w) * 200` and `py = (1 - y/w) * 120`, with (0, 0) at the top left.
- **Values outside the Laverre Pokemon Center:**
  - fovy is 30 degrees. Row 1 is 3.732 times the view's up vector and row 0 is 2.2392 times its right vector, so
    their ratio is 400/240;
  - the pitch is 40.7 degrees (0.711 rad);
  - the distance is 254.45, to a target 12 units above and 10.3 north of the player;
  - the eye is at (8667, 322.1, 9317.3) for a player at (8667, 144, 9135).
  - The camera block also holds the eye and target (+0xBDC, +0xBE8), the forward vector, the yaw (-pi), the pitch and
    the distance. They were not needed, so their offsets are given only as seen.
- **The check in `camera()`:** the 16 values are finite, row 3's direction part is a unit vector, and |row 1| / |row 0|
  is 400/240.
- **Why memory, not the GPU uniforms:** xr-net reads game memory and has no access to the renderer. The block is the
  game's own and is updated every field update. Its pointer path held on every map tried: 202, 200 and 188.
- **Validated:**
  - At "outside", a tile left of the door, the player's position projects to (199.9, 147.9). The screen shows the
    feet at about (200, 151).
  - An NPC (id 0x0A) at (8595, 146, 9189) projects to (86.0, 224.6). Its feet are seen at about (86, 229).
  - In the live test, the projected point is 0.1 px from the player's sprite: the pixels that change when the player
    turns on the spot. It is 2 px from the sprite's median x, and between the sprite's top and bottom.
- **Do not search memory for "a projection matrix":** use the block. Other matrices in memory look like the camera
  but are not this frame's:
  - a projection with fovy 18.3 degrees (3.7248, 6.2205);
  - copies of the view that stay put while the player moves.

## Still open

- Surf (its form and motions; a surfing player's puppet is hidden until then), and the bike model on a puppet: see
  "Movement modes" and "The bicycle on a puppet".
- Which save UI flow each caller of 0x433B20 (0x1D3B98, 0x1D4060) serves. Both run on the main thread (see Threads).
- The zone NPC loader is also called from DllField (stub DllField+0x4718, call at DllField+0xA3D2C).
  - That path reaches Create through 0x3D6E9C, which the Create hook covers.
  - DllField's own Create calls (the player builder) do not go through the hook.

## Zone warps, triggers and the zone table (p9-story)

Read from romfs `a/0/1/2` (a GARC; each file LZ11, "ZO"), section 1 at the offset in the file's +0x08, after the
furniture (0x14 bytes each) and NPC (0x30 bytes each) entries counted in the section header:

| Entry | Size | Fields |
|---|---|---|
| Warp | 0x18 | +0x00 u16 destination zone, +0x02 u16 the destination's warp index, +0x06 u16 placement (0 grid, 1 rail), then for grid: +0x08/+0x0A/+0x0C u16 x, y, z in world units (tile = unit / 18; overworld zones use the big world frame, interiors small numbers), +0x0E/+0x10 u16 width and height in tiles; for rail (Lumiose's streets, zones 63-69): +0x08 u32 a rail anchor, +0x0C/+0x0E i16 offsets, +0x10/+0x12 i16 width and height (may be negative) |
| Trigger | 0x18 | +0x04 u16 script, +0x0C/+0x0E u16 tile x, z, +0x10/+0x12 u16 width and height |

The last file of the GARC is the zone table: 0x38 bytes per zone (360 zones); the u16 at +0x1C holds the location
name in its low 9 bits (the even met-location numbers: 6 Vaniville Town, 12 Route 2, 14 Santalune Forest, 16 Route 3,
18 Santalune City, 20 Route 4, 22 Lumiose City). Checked: zone 286 (Santalune Forest) has warps to 259 (Route 2) at
tile 38,76 and to 260 (Route 3) at 7,8; walking there crosses into zone 260 at its own warp back (506,635). Zone 54's
NPC models match the Laverre Pokemon Center's (zone 202), which identified Santalune's Pokemon Center.

**Lumiose's rails.** Lumiose's NPCs use the same split: zone NPC entry +0x24 u32 is 0 for grid placement (+0x28/+0x2A
tiles; zone 67's NPC 1 at tile 93,29 stood at 1683,531 = (93.5, 29.5) x 18 after a Fly) or 1 for a rail (+0x28 u32
anchor). The anchors are shared: zone 67's warp to zone 93 and its NPC 4 both use 0x020293D3. The anchor is a
little-endian u32 (zone 63's door to zone 83 is bytes `b7 fe 05 03` = 0x0305FEB7; run 3 wrote 0x0503FEB7). After Fly
the player stood at 1714.1, 2313.55 in zone 67, off the tile grid. The loaded zone data sits in the heap as stored
(zone 67's NPC entries at 0x08CF1218 and warps at 0x08CF1338 in that run), not converted.

**The rail-to-world conversion (story run 4, time-boxed 1.5 h: found and called, the rail data itself not decoded).**
- code.bin **0x3AF1A4(warp, out)** asserts placement (+0x06) == 1 and builds a rail location: the u32 anchor (+0x08),
  9, then (A + W/2) x 18 and (B + H/2) x 18 as floats from the i16s at +0x0C/+0x0E/+0x10/+0x12 (+-9 when W or H is
  even), rounded by code.bin 0x3C9E98. **0x3AF2C8(warp, location)** wraps it (location type 1 = rail; a grid warp gives
  type 0 with its tile position).
- The warp hit test **code.bin 0x3AEC4C(warp, player pos, ?, ctx)**, called from DllField+0x1E5C8 through its import
  stub (ctx = [controller + 0x34]), turns the location into x,y,z through three code.bin stubs that DllField fills at
  load: **0x19AA44** -> DllField+0x8AEB8 (converter, [[ctx+0xFC]+0x7C], [ctx+0xFC]); **0x19AA4C** -> DllField+0x89A4C
  (converter, location, 1); **0x19AA54** -> DllField+0x89C20 (converter, x,y,z out), which evaluates the rail through a
  virtual call on the object DllField+0x82650 builds.
- **`XR_RAILPOS=ANCHOR:A:B:W:H;...`** (runtime debug aid in `hooks.rs`) makes the same calls from the per-frame hook
  every 600 ticks and logs `railpos ...: x,y,z (tile tx,tz)`.
- **The two data points check out.** Zone 67's Pokemon Center door (0x0203E3CD +3,4, -1x4) gives 1704.1, 2346.7 =
  tile 94,130 (run 3 measured about 94,128 after a Fly; the warp is 4 tiles deep). Zone 63's door to zone 83
  (0x0305FEB7 +11,-11, 5x1) gives 2491.1, 4072.0 = tile 138,226 (run 3's explorer: about 139,224).
- **Limit:** only rail lines loaded around the player resolve; the others come back 0,0,0 (the lab's 0x020293D3 from
  anywhere in zone 63; 0x0305FEB7 from the Route 4 gate). Positions used in run 4: the Route 4 gate warp 225,215;
  zone 63's warps to 66 at tiles 232-246, 154-163; the Route 5 gate (zone 64's 0x01049366 +12,-28) 949,3867; Cafe
  Soleil's door (0x0104CDF2 +10,11) 1026,3020; the boutique (zone 124) 1175,3212.
- **The lab of the first visit is zone 83**, not 93/96: zone 63's warp to the lobby (83), its lift to 84 (2F) and 85
  (3F, Sycamore). Zones 93/96 off zone 67 are the later lab. Other South Boulevard doors: 75 the Pokemon Center, 88
  Coiffure Clips, 107 the PR Video Studio, 141 a restaurant (a double battle at its door trigger), 124 (off 64) the
  boutique (DllSalon, DllKisekae), 125 (off 64) Cafe Soleil.

# X v1.0: the frame rate (Frame rate setting, xr-runtime rate.rs)

How the field runs at 60, 120 or 240 updates a second at the game's own speed. Found with XR_WATCH (interpreted runs),
XR_MEMDUMP diffs at consecutive updates, XR_PEEK and rizin on code.bin and DllField.cro, from the 100% save outside the
Laverre City Pokemon Center. DllField addresses are as loaded at 0x006A5000 (text at +0x180).

## The game's own 30 and 60

- The frame controller (0x6C bytes at 0x08C61A78, pointer at code.bin 0x5E9E90): +0x0C u8 pending rate request
  (2 none), +0x0D u8 mode (1 every other pass a full one, 30 Hz; 0 every pass, 60 Hz), +0x0E the phase toggle.
- code.bin 0x108A88 (once a main-loop pass): a pending request that differs from the mode is taken: code.bin 0x110DC0
  (and 0x110EE4 going to 1) writes the mode into the listeners' +0x14, then +0x0D = request, +0x0C = 2.
- The listeners (singleton at [code.bin 0x5EB50C], lists at +0x0C, +0x18, +0x24, +0x30) are the display and render
  target objects (vtables 0x597860, 0x597A10, 0x5977F4): their mode is the swap interval. **No game logic listens**:
  with request 0 in the field the player walked 2.5714 a refresh, twice the 30 Hz speed, exactly as with the mode
  forced. Everything below is scaled by RE:DEX.
- A scene asks for its mode at scene changes (code.bin 0x10E498, from the proc's vtable +0x18); the field, the X menu
  and battles ask for 1.

## Who moves what, an update at a time

| What | Where | How it keeps time | RE:DEX at S = rate / 30 |
|---|---|---|---|
| A move model's action | mm +0x8FC u16 code, +0x8FE u16 direction, +0x900 u32 state (1 playing, 2 ended) | | |
| Walk step (0x12), run step (0x0D) | mm +0x904 u16 length (7 walk, 4 run; NPCs walk 7 too), +0x906 i16 done | DllField text+0x4EF90 (0x6F4110): done += 1, position = start + (target - start) * done / length (0x6F4148 divides), ends at the length. Set by text+0x79AC0 (0x71EC40) | length S times longer; an action seen one update in gets the rest of its way on a finer count (length * (S * length - 1), the game's 1 plus length - 2 a pass) so the total is S * length updates with no stall |
| Turn on the spot (0x17) | mm +0x906 i16 updates left (2, counted down at 0x6EF4D4) | the controller looks at the d-pad 2 updates after it ends; held then, it walks | (2T + 3) * S / 2 - 2 updates: a tap of 6 frames turns, 7 walks, as at 30 |
| Motion frame | [mm +0x97C] +0x18 f32 frame, +0x1C the last, +0x20 length (14 for the walk cycle), +0x24 speed | code.bin 0x39664C: frame + speed each update, the speed set again every update (0x3A9834 from a DllField function behind code.bin's stub 0x19AF14) | the frame's step divided by S after each update (wrapping at the length) |
| NPC wander wait | mm +0x8C4 u16, counts down an update at a time | | one count in S |
| The bike | controller (snapshot +0x14) +0x38 f32 velocity, set again every update; the position moves by it | | the player's position step divided by S after each update (mm +0xEC and its copy at +0x114) |
| The skates | controller +0x38..+0x40 velocity, changed each update (speeding up, slowing down); counters +0x48 (down), +0x54, +0x58 (up) | | the velocity's change 1/S, the position's step 1/S scaled by the velocity's cut; the counters one count in S |
| Controller counters | +0x1A u16 standing still, +0x44 u32 since the last step | +1 an update | one count in S |
| The camera | camera block (player field object +0xF0): +0xD94 the point it follows, +0xDF8 the point that one eases to, gains at +0xDBC, +0xDD8, +0xE20, +0xE3C (three each) | eased each update (DllField 0x72C87C; the game's 60 Hz mode eases by 0.843 an update where 30 does 0.770: it half follows the mode) | each update's change to all of them divided by S * 0.78 (tuned in Laverre: the settle and the walking lag within 5% of 30 at 60, 120 and 240) |
| Walk velocity | controller +0x38, set once a step (DllField 0x716B18, literals 0x716AD8..0x716DAC by action code) | not used for the step itself (patching it changed nothing) | none |

- code.bin's literal pools are folded into xgame.dll's code (read-only text); DllField's are loaded at run time (module
  mode), but the speeds and lengths that matter are immediates or recomputed every update, so they are scaled
  where the game keeps state between updates instead.
- The player snapshot's u32 at +0x48 has 0x100 set while the X menu is open (and the move models' status 2 gets 0x8):
  the field goes back to 30 then.
- Events (doors, talking, scripts): the field's touch screen object ([player field object +0x16C]) has u8 +0x91 = 0
  while one runs (DllField 0x6D32D4 clears it through 0x729E9C as the event starts). The field goes back to 30 for
  them, so script waits, fades and text keep the game's own time; a step that was stretched goes back to the game's
  count (done * len0 / len), and one begun at 30 that goes on at S has its remaining updates stretched
  (S * (len - done)). Measured: through the Laverre Pokemon Center door and back out, every rate matches 30 at each
  checkpoint (XR_SCRIPT_CLOCK=console, below); talking to the nurse switches to 30 with the A press.
- XR_SCRIPT_CLOCK=console: a headless script's frames count console refreshes of time (cycles / CYCLES_PER_FRAME), so
  one script runs at the same pace in time at every rate, even through scenes at 30.

## Native hooks for what code.bin animates (hooks.txt 5 to 7)

What code.bin itself steps each update is cut inside the call, by hooks that set the step to 1/S, run the function and
put the step back (rate.rs `cut_*`, while the field runs at S; at 30 they only run the function):

| Hook | code.bin | What it steps |
|---|---|---|
| 5 `virtual` | 0x2ECD20 | a scene node's animations: its +0xDC list (step +0x25C, frame +0x254) and +0xCC list (step +0x16C, frame +0x164), then its children (+0xBC, through 0x2D2B5C). Snowbelle's falling snow is these: with the hook, none of its 320 values moves faster than at 30 |
| 6 `virtual` | 0x39664C | a model's motion: frame +0x18 plus speed +0x24 (the speed set again each update) |
| 7 | 0x45B734 | the environment's 8 slow drifts: each 0x24 bytes, an angle at +0x18 and a value at +0x1C going on by the speed at +0x20 |
| 9 `virtual` | 0x369F08 | an animation set: count +4, list [+8]; each animation's frame +0x28 goes on by its speed +0x30 (wrapped or held in +0x34..+0x38). The battle's camera and stage animations (a frame at 0x0853E0EC from the wild-battle state) |
| 10 `virtual` | 0x14DEC4 | a layout's animations (nw::lyt screens: menus, the battle's bottom screen, the message window): count +0xAC, 0x2C-byte entries at [+0xA8], flags +0..+2, the animation at [+8] (frame at its +0x10), speed +0x1C |
| 8 `virtual` | 0x3D0458 | a move model's step along its rail, (s0, s1) its two parts: Lumiose's rail walks. DllField's rail action (0x6FC1BC: the velocity at its +0x24, +0x28, eased at +0x18, +0x1C) calls it once an update through its import stub 0x6A7910, so `virtual` catches it |

- 0x2ECD20 is only ever called through its class's vtable (code.bin 0x595AE8), and generated code calls a hook only
  at direct call sites. A hooks.txt line can now say `virtual`: xr-recomp's `xr_lookup` then hands out the hook for
  that address too (the native code's indirect calls and interp_entry go through it), and the runtime runs the
  function itself through the new export `xr_lookup_native`.
- A DLL's hook list must be a start of the runtime's HOOKED (game.rs): a DLL built before these hooks (the first 5)
  still runs with this redex, without the cuts (rate.rs then cuts the move models' motion frames itself, after each
  update); the new DLL (8 hooks) needs this redex. The hooks' addresses (and so the hook-list hash) do not depend on
  `virtual`.
- Found with an idle scan: memory dumps 60 frames apart at 30 and at the rate, every float whose change is the rate's
  multiple of 30's. Laverre, idle: 54 such values without these hooks, 11 with
  (the 2 left are the lighting's clock, [field +0x50] +0x2C +0x48, a counter of updates within the minute, and 9 copies
  of a value that follows it).

- A `virtual` hook also catches CRO code's calls into code.bin: their import stubs (`ldr pc, [literal]`) are indirect
  branches, which go through `xr_lookup`. Measured in Lumiose (XR_SCRIPT_CLOCK=console): a rail walk goes 12.13 units
  every 10 console frames at 30, 60, 120 and 240; the walk starts up to 2 frames sooner over 30 (the d-pad is read every
  update, not every other frame) and stops within a 30 Hz update's share of 30's spot, and a door's fade to the next map
  takes 99 frames at 60 against 101 at 30.

## Battles (at the setting's rate; XR_RATE_BATTLE=0 keeps them at 30)

- Tools: XR_THUMBS=1 writes each refresh's console time and both screens in grey 8x8 blocks to logs/thumbs.bin;
  matching each 30 Hz frame with the 60 Hz run's best match in time gives the lag along the battle (a falling lag: 60
  runs ahead). XR_RATE_TRACE=1 logs each pass's mode and the motion hook's objects; XR_RATE_SLOW=ADDR,... (hex) counts
  those u32s one in S (debug).
- From the wild-battle state (Route 2, frame 4790 of wild-battle.txt; the intro after "A wild Zigzagoon appeared!"),
  with hooks 6, 9 and 10 cutting: the motions step 0.5 an update (the trainer's throw motion keeps 30's speed), yet
  the lag still falls 0.5 a frame from the start (the text's wait, the throw's start, the ball, the flash and the send
  out all come at twice the pace). What times the sequence is not found: in two memory dumps 10 and 20 console frames
  apart (0x00600000..0x18000000) the only counters of updates are the Pokemon models' (0x083E7094, 0x084208C4: code.bin
  0x447F58), the system's (0x08C3B994: code.bin 0x1114F4; 0x08D7FE14: 0x122348; counting them one in S changes nothing)
  and DllBattle's model object (vtable 0x77FC68, frame +0xD4 += +0xD0 at DllBattle 0x703D34). Next: watch the
  message window's wait and the sequence's command pointer (a sequence file read pointer moving on is easier to find
  than a counter), or trace DllBattle's calls per update at 30 and 60 (XR_INTERP_MODULES=DllBattle).

- Found since (XR_CALLS=1 logs each interpreted call's first time with its frame; the 30 and 60 runs' event frames
  side by side give the lag at each event): the sequence is a set of waits, each on a per-update step of its own.
  Fixed for the intro (state to the command menu within 2 frames at 60): the main loop's request queue (code.bin
  0x111F3C, one request a pass; DllBattle waits for it to empty), the trainer object's own frame (DllBattle 0x6DB5B0:
  +0x1A4 += +0x1AC, cut where it calls code.bin 0x37E398), the task lists (code.bin 0x38FEC0: the scene's sequence
  DllBattle 0x721104, countdowns 0x71BD58 and 0x71D8D0, camera moves 0x71C4AC and 0x71F1E4), the timeline player
  (code.bin 0x3AD190: +0x14 the frame), the Pokemon's blinks (code.bin 0x441BC0: +0x330 counts 2 an update while
  +0x3ED is set) and two of the battle's state functions (DllBattle 0x6B5D88, the command screens; 0x6A9668, the
  client), hooked as CRO functions (hooks.txt `DllBattle+0xOFF virtual`, handed out by xr_lookup).
- Since: the task lists run their move tasks (DllBattle 0x71C4AC, 0x71F1E4, 0x71D7B8, 0x71DEC0: camera and model
  moves reading integer-step interpolators, DllBattle 0x7718FC vector, 0x77170C and 0x771780 scalars) every pass
  with the interpolators' step and length times S; the other tasks one pass in S. The intro's camera moves change the
  top screen on nearly every refresh at 60 (XR_THUMBS, per-refresh change count).
- The move turn ran fast because the scene's message windows (DllBattle 0x748BCC, the manager at [scene+0x190]) run
  code.bin's text printer (vtable 0x599A60, its update 0x3F5AB0: characters, the wait after the text) every pass.
  Hook 18 (the main update DllBattle 0x6BB024) skips them on the passes that are not one in S (manager +0x34 cleared
  for the pass) and swaps every state function of the battle's state machine and clients for one returning 0 there.
  Hook 19 (the battle's proc DllBattle 0x6D80D4) runs its end phase (+0x40 = 2) one pass in S. From the command
  menu, a won battle (Tackle, HP, the foe's faint, EXP, the end) keeps 30's timing within 4 frames at 60; the intro
  within 2 at 60, 120 and 240 (event frames at 120 and 240 count refreshes: rate/60 to a console frame).
- Not right yet: layout animations cut to 1/S change the picture only on whole frames, so the menus (the command
  menu's slide) still move in 30 Hz steps; the timeline (hook 14) counts whole frames and stays one pass in S.
- Presses (hook 20): the input devices' update (code.bin 0x3570C0, from the main loop through its vtable, before
  TICK) works out a press on the first pass that sees the button down, and only for that pass; the battle's menus read
  it on their one pass in S, so most presses were lost (at 60 one in two; the scripts' presses happened to land on the
  battle's passes there, at 120 and 240 never). In a battle at the rate the update runs only before the battle's
  passes. From the battle's start through the intro, a move turn, the faint, EXP and back to the field: within 3 to
  5.5 frames of 30 at 120 and 240, 1 to 2.5 frames ahead of 60 (presses are taken sooner); no faults.
- Still at 60, 120 and 240: a 14-frame stretch at the battle's end (30's frames 5843 to 5857 of the mixed run, after
  the last EXP press) moves twice as fast, its events still within 3 frames.
- A battle runs at the rate only between its loads and its end: the proc in its battle phase (+0x40 = 1) and the
  scene's state machine (DllBattle 0x71A210, state u8 +0x694) past its first state's loads (that state, DllBattle
  0x6E446C, counts steps at +0x6B8; 0x14 is the intro). Run at the rate, the setup's waits ran at twice the pace;
  cut to one pass in S, its loads came 9 frames late (the main thread's extra passes slow the loader). Setup and
  teardown at the game's 30 keep both. From the battle's start (wild-battle.txt frame 4500), through the intro, the
  move turns, the faint, EXP and the end, back to the field, the X menu and the field again: within 5 frames of 30.
- The field after a battle: its touch screen (the player object's +0x16C) stays locked (u8 +0x94) until its menu
  shows, then the menu slides in by its own steps (u32 +0x9C counts updates since it showed). The field stays at 30
  while locked and for 12 updates after.

## Y

Y's code.bin differs from X's in 17 words, none at the hooked functions or the frame controller's code; its
DllField has the step counter, the camera's follow, the turn, the controller's state and the touch screen's event
switch at the same addresses (the map animations' loop 4 bytes on; rate.rs uses only object layouts, not DllField
addresses). Measured on Y with the 100% save in Laverre: walk distance over 10 tiles' time 180.00 at 60, 120 and
240, the 6/7-frame tap, and the Pokemon Center door in and out matching 30.

## Pacing over 60

The field at mode 0 does one full pass a vblank, so 120 and 240 shorten the emulated refresh (System.frame_cycles,
CYCLES_PER_FRAME * 60 / rate) while the field runs at mode 0; the window paces refreshes by their cycles. The clock,
the timers and the DSP keep counting in CPU cycles, so time and sound stay real.

## Not done yet (the field)

- Surfing and ledges (not reached from the saves used here), NPCs on rails, the lighting's clock (above).
- The skates' cruise is 1.5% (60) to 5% (240) slower than 30, the glide after letting go 10 units longer.

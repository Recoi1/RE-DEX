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
| 6 | 0x39664C | a model's motion: frame +0x18 plus speed +0x24 (the speed set again each update) |
| 7 | 0x45B734 | the environment's 8 slow drifts: each 0x24 bytes, an angle at +0x18 and a value at +0x1C going on by the speed at +0x20 |

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

- Lumiose's rails: a rail step (action 0x96) moves 2.57 an update at every rate, so S times as fast. The move
  model's rail position (+0xF4) and the controller's (+0xC40) are copies made from a step value written through
  code.bin 0x3D04B4 from DllField 0x6FC2C0; scaling either (or the move model's +0x830 step count) changed
  nothing, so the step's own state is elsewhere. code.bin 0x3E1C40 (object at [field +0x50] +0x4C) sums the
  distance walked, from the position: it follows by itself.
- Surfing and ledges (not reached from the saves used here), NPCs on rails, the lighting's clock (above).
- The skates' cruise is 1.5% (60) to 5% (240) slower than 30, the glide after letting go 10 units longer.

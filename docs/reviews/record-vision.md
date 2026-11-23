# Review: record, vision and OCR

Card #700, 2026-11-23. This is a fault list. It changes no code. The lead
makes the fix cards from it, the worst finding first.

Line numbers are those of commit `a1b4b985` (`next` at the start of the
card).

## How the review was done

The review is by reading. No test was written or run for a finding, so each
finding is a claim from the code, and the proposed test is what proves or
refutes it. No proposed test uses a wall-clock limit. Where a test must show
that something never returns, it runs under
`#[tokio::test(start_paused = true)]`: the paused clock jumps to the timeout
as soon as every task is parked, so the limit is not a wall-clock one.

Severity words are those of the card: data loss, crash, security against a
hostile guest or peer, wrong result, cosmetic. "Confidence" says how sure the
reading is.

## Who can reach what

- The recorder, the capture, the matcher and the OCR all run inside the lab
  daemon, which serves every machine of the lab. The matcher and the OCR
  also run in the script runner, which is in the same process.
- A guest alone controls the screen size up to the limits of the two display
  clients (`src/vnc.rs:77-91`, `src/hvdisplay.rs:66-72`: 16384 a side,
  256 MiB a buffer), the picture, how often it changes, and the cursor shape
  and its size. Both clients accept a cursor of 0 x 0
  (`src/vnc.rs:504-509`, `hv/crates/vmlab-hv-display/src/protocol.rs:361-369`).
- The options of a recording, the match threshold, the regions and the
  reference images come from the person who runs the lab or wrote the
  script. A fault that needs such a value is a robustness fault, not a
  security fault.
- `ffmpeg` and `tesseract` are host programs found on `PATH` (or the host
  config for ffmpeg). They read what the guest painted: ffmpeg as raw frames
  that vmlab composed, tesseract as a PNG that vmlab encoded. No file that a
  guest wrote is given to either program, and the `image` crate is built
  with `png`, `pnm` and `bmp` only (`Cargo.toml:94`).

## What was read and what was not

Read in full, without the test modules:

- `src/record/mod.rs`, `core.rs`, `encoder.rs`, `executor.rs`, `input.rs`
- `src/vision/mod.rs`, `screenshot.rs`, `ocr.rs`, `matching.rs`
- `src/labd/record.rs`
- `src/scripting/ocr.rs`

Read in part:

- `src/labd/display/mod.rs`: `grab`, `grab_frames`, `screenshot`, `ocr`,
  `find_image` (100-170, 295-345).
- `src/scripting/mod.rs`: `wait_for_text`, `match_opts`, `find_once`,
  `wait_for` (395-435, 545-603), and `cancel::block_on`
  (`src/scripting/cancel.rs:100-115`).
- `src/hvdisplay.rs`: `Surface::configure`, `NativeStream`, `decode`
  (60-140, 257-414).
- `src/vnc.rs`: the stream rectangle and the cursor decoding (286-345,
  504-520).
- `src/labd/mod.rs:755-783` (the `machine.ocr` and `machine.find_image`
  requests).

Not read:

- `src/cli/record.rs`, `src/scripting/record.rs` and the `--record` flags of
  the start verbs. `docs/reviews/wscript.md` read `scripting/record.rs`.
- `src/record/core/tests.rs` and the other test modules, except to see that
  no test gives the core a cursor of size zero.
- ffmpeg, tesseract and the `image` crate themselves.
- The rest of `src/vnc.rs` and `src/hvdisplay.rs`: `docs/reviews/display.md`
  covers them.
- What the lab daemon does with a panic in a request handler or in a spawned
  task. F1, F2 and F4 say "panics" and stop there.

## Findings, worst first

| Code | Severity | Fault |
|------|----------|-------|
| F1 | data loss, security against a hostile guest | A cursor of size zero panics the recorder and the recording is lost |
| F2 | crash of the lab daemon | The recorder holds every overdue frame in memory, with no limit |
| F3 | crash (a stop that never returns) | A stalled ffmpeg blocks `record stop`, `down` and `destroy` |
| F4 | wrong result | Two captures of one machine share one scratch file |
| F5 | wrong result | The matcher returns the best of 8 coarse candidates, not the best match |
| F6 | wrong result | The match score ignores colour, brightness and contrast |
| F7 | security against a local peer | The OCR scratch file has a predictable name in the shared temp directory |
| F8 | wrong result | A cancelled or timed-out OCR leaves tesseract running and its file behind |
| F9 | crash of a request | A large duration panics `Settings::resolve` |
| F10 | wrong result | The match threshold is not checked |
| F11 | wrong result | A text match carries the click point (0, 0) |

### F1. A cursor of size zero panics the recorder and the recording is lost

- Where: `src/record/core.rs:606-632` (`draw_cursor`), reached from
  `compose` (`530-534`). The shape arrives through `cursor` (`288-291`) or
  `rfb_cursor` (`295-307`).
- Fault: `draw_cursor` computes `u32::from(cursor.height) - 1` and
  `u32::from(cursor.width) - 1` and then reads `cursor.rgba[i + 3]`. For a
  shape of 0 x 0 the loop still runs one time, because the drawn width and
  height are `max(1.0)`. In a debug build the subtraction panics. In a
  release build it wraps, the index is 0, and `rgba[3]` of an empty vector
  panics. Nothing between the display client and the core refuses the
  shape: the native protocol accepts width 0 with height 0
  (`protocol.rs:361-369`), `decode` passes it on (`src/hvdisplay.rs:388-406`),
  and the RFB client accepts any side from 0 to 512 (`src/vnc.rs:504-509`),
  so 0 x 5 passes there too.
- What follows: `compose` runs in the clock task and in the reader task
  (every event calls `advance`). The reader's panic poisons the mutex, which
  every user ignores with `into_inner`, so the clock task then panics in its
  own `compose`. The recorder task ends by panic. Its `Encoder` is dropped,
  and `kill_on_drop(true)` (`src/record/encoder.rs:204`) kills ffmpeg before
  it writes the trailer. An MP4 written with `+faststart` has no index then
  and does not play. `Recorder::stop` reports `recorder task failed`.
- Reach: the cursor is drawn only when a pointer position is known and
  shown. On the native stream the guest's own pointer reports give one. On
  RFB one click or move sent by vmlab gives one. vmlab-hv's display device
  lets the guest set width 0 and height 0
  (`hv/crates/vmlab-hv-devices/src/vdisplay.rs:292-299` refuses only above
  256). I did not follow whether the server sends that shape or drops it.
- Severity: data loss (the recording), security against a hostile guest: a
  guest can end the recording of its own screen.
- Proof: unit test `a_cursor_of_size_zero_draws_nothing` in
  `src/record/core/tests.rs`. Make a `Core`, call `connected(0, 32, 16)`,
  `cursor(at, CursorShape { hotspot: (0, 0), width: 0, height: 0, rgba: vec![] })`,
  `pointer_state(at, 3, 3, true)`, then `tick(1 s)`. Assert that it returns
  frames. Today it panics. Add the case width 0, height 5 through
  `rfb_cursor`. A second test at the socket: `hvdisplay::fake` sends a
  `Cursor` of 0 x 0 during a recording, and the test asserts that
  `Recorder::stop` returns `Ok`.
- Confidence: high for the panic. Medium for the guest reaching it on
  vmlab-hv.

### F2. The recorder holds every overdue frame in memory, with no limit

- Where: `src/record/core.rs:374-377` (`tick`), `403-408` (`advance`),
  `452-464` (`close_idle`), `src/record/executor.rs:196-207` (the clock
  loop), `301-306` (the reader calls `advance` through every event).
- Fault, part 1: the clock loop writes the frames of one tick to ffmpeg and
  only then takes the next tick. `Core::tick(now)` composes one full frame
  for each interval that ended, all into one `Vec`. One frame is
  `width * height * 3` bytes: 6.2 MB at the default size, 99.5 MB at the
  largest size that `Size::validate` accepts (`src/record/mod.rs:114-126`),
  at up to 120 frames a second. When ffmpeg takes frames slower than the
  clock makes them, each batch is larger than the one before, and the
  growth has no bound. No frame is dropped and the recording is not ended.
- Fault, part 2: while the clock task waits in `write_frame`, the reader
  task still calls `advance` for each display event, and each call pushes
  the frames that became due into `Core::out`. A stalled ffmpeg (see F3)
  with a guest that paints makes `out` grow by `fps` frames a second: 186 MB
  a second at the defaults.
- Fault, part 3: with idle compression, an idle run that ends at or below
  the threshold is written out whole. `close_idle` makes
  `len / dt` deep copies of one frame in one step
  (`std::iter::repeat_n(frame, frames)`). The threshold is the caller's
  number of seconds with no upper limit (`src/record/mod.rs:207-215`). With
  `compress_idle` 600 at the defaults, an idle run of 599 s asks for
  17970 frames, 111 GB, in one call. At the default threshold of 2 s it is
  60 frames, 373 MB, each time a short idle run ends.
- What follows: the memory belongs to the lab daemon. An allocation that
  fails aborts the process, and the kernel may kill it first. Every machine
  of the lab loses its daemon.
- Reach: a guest cannot set the options. It can make the encoder slower
  (a full-screen picture that changes in every frame is the slowest input
  for x264 and VP9) and it controls when an idle run ends. A slow or full
  output disk and a loaded host do the same without a guest.
- Severity: crash of the lab daemon. Security against a hostile guest where
  the encoder is near its limit.
- Proof: three unit tests in `src/record/core/tests.rs`, all without I/O.
  `a_late_tick_does_not_return_an_unbounded_batch`: fps 30, size 16 x 16, no
  compression, one rectangle, then `tick(600 s)`. Assert that the returned
  `Vec` is not longer than a bound that the fix names. Today it has 18000
  frames. `events_do_not_pile_frames_while_nobody_ticks`: call `rect` with
  `at` from 0 to 600 s and never `tick`. Assert the same bound on what the
  next `tick` returns. `a_long_idle_run_below_the_threshold_is_not_cloned`:
  `compress_idle` 600 s, one rectangle at 0, one input at 599 s, `tick`.
  Assert the bound. Today it returns 17970 equal frames.
- Confidence: high for the counts. Medium for how soon a real encoder falls
  behind at the default settings.

### F3. A stalled ffmpeg blocks `record stop`, `down` and `destroy`

- Where: `src/record/executor.rs:205-207` and `248-250` (`write_frame` is
  awaited outside the `select!` that watches the stop signal),
  `src/record/encoder.rs:199-204` (stderr is a pipe), `229-240` (`finish`
  waits for the exit and reads stderr after it).
- Fault: the stop signal is seen only between ticks. While the clock task
  waits in `write_frame`, `Recorder::stop` waits for the task, and no path
  kills ffmpeg. `record_finalise_machine` and `record_finalise_all`
  (`src/labd/record.rs:272-288`) wait for `stop`, and `vm stop`, `down` and
  `destroy` call them before the machine goes. ffmpeg stops reading its
  stdin when its output file is on storage that hangs, when the process is
  stopped, or when it has written more to stderr than the pipe holds:
  nobody reads that pipe before ffmpeg exits. `finish` has the same order
  problem by itself: it waits for the exit first, so an ffmpeg that writes
  more than a pipe of errors while it closes the file never exits.
- Severity: crash class (the stop verbs never return, and F2 part 2 fills
  the memory in that time). No guest reaches it directly.
- Proof: test `stop_returns_when_the_encoder_stops_reading` in
  `src/record/executor.rs`, with `start_paused = true`. The "ffmpeg" of the
  spec is a script that never reads stdin (`exec sleep infinity`). Start a
  recording against `vnc::fake`, then await `Recorder::stop` inside
  `tokio::time::timeout`. Assert that `stop` returns (an error is fine).
  Today the timeout fires. A second test, `finish_reads_stderr_before_it_waits`:
  the script writes 1 MiB to stderr, then reads stdin to the end and exits 1.
  Assert that `stop` returns the error with ffmpeg's text.
- Confidence: high for the order of the awaits. Low for how often ffmpeg
  fills its stderr at `-loglevel error`.

### F4. Two captures of one machine share one scratch file

- Where: `src/labd/display/mod.rs:137-147` (`grab_frames`), with
  `src/labd/hypervisor/vmlab_hv/display.rs:70-73` and
  `src/labd/hypervisor/qemu.rs:333-335` as the writers.
- Fault: each capture writes `<run dir>/.grab-<machine>.ppm`, reads it and
  removes it. The name is the same for every capture of the machine and no
  lock is held. Neither writer writes to a temporary name and renames. Two
  captures at one time give one of these: the first reads a file that the
  second has truncated and half written, and fails with `failed to decode`,
  or the first removes the file before the second reads it, and the second
  fails with `failed to read image file`. On an error the file stays
  (`load_screen(&tmp)?` returns before `remove_file`).
- Who makes two captures: `screenshot`, `ocr`, `find_image`, each round of
  `wait_for` and `wait_for_text`, and every pointer action, because `size()`
  (`155-158`) takes a full capture to scale the coordinates. Two provisions
  or handlers on one machine do it, and so does `vmlab vm screenshot` while
  a script waits for an image.
- What follows: `find_once` and `wait_for_text` return the error with `?`
  (`src/scripting/mod.rs:410`, `567`). The wait does not try again: the
  script step fails on a screen that was fine.
- Severity: wrong result (a false failure of a script step or a verb).
- Proof: test `two_captures_at_once_both_decode` in
  `src/labd/display/tests.rs`. The `Frames` double writes a valid PPM in two
  halves and waits on a gate between the halves and after the second half.
  Start capture A and let it finish its write. Start capture B and stop it
  after the first half. Let A continue. Assert that A returns an image.
  Today A fails to decode. Second order: let B finish its write, let A read
  and remove, then let B continue. Assert that B returns an image. Today B
  fails to read.
- Confidence: high for the vmlab-hv writer (a synchronous `save_png` on a
  multi-thread runtime). Medium for QEMU, where the time between the end of
  `screendump` and the read is short.

### F5. The matcher returns the best of 8 coarse candidates, not the best match

- Where: `src/vision/matching.rs:282-339` (`search_best`), `373-401`
  (`top_candidates`), with `TOP_CANDIDATES = 8` (`26`).
- Fault: for a template of 8 pixels or more a side, only the 8 best
  positions of the 4x reduced picture are refined, and the result is the
  best of those. The coarse score of a true occurrence depends on where it
  lies on the 4-pixel grid. An occurrence that starts 2 pixels off the grid
  has each coarse pixel averaged over two neighbour blocks of the template,
  and a template with fine detail (small text, a thin icon) scores low
  there. When the screen also has 8 other places that score higher in the
  coarse pass, the true occurrence is never refined. If one of the 8 passes
  the threshold at full resolution, it is returned as the match, with a
  lower score than the exact one. If none passes, the result is `None`
  while the template is on the screen.
- What follows: a script gets the position of a similar element, for
  example another row of a list or another button of a toolbar, and clicks
  it. Or `wait_for_image` times out on a screen that shows the image.
- Severity: wrong result. A script acts on the wrong place.
- Proof: unit test `the_exact_occurrence_beats_eight_near_copies` in
  `src/vision/matching.rs`. Build a 16 x 16 template from the block texture
  of `test_screen` (one random level for each 4 x 4 block). On a flat grey
  screen put 8 copies of it at positions that are multiples of 4, far
  apart, each changed by `xor_noise` so that its full score is about 0.95.
  Put the exact template at a position with x and y equal to 2 modulo 4.
  Assert that `find_template` returns that position with a score above
  0.999. The reading says that it returns one of the 8 copies. A second
  case with the copies at score 0.85 and threshold 0.9 asserts `Some`,
  where the reading says `None`.
- Confidence: medium. The test was not run, and the coarse score of the
  shifted occurrence is estimated, not measured.

### F6. The match score ignores colour, brightness and contrast

- Where: `src/vision/matching.rs:121-129` (`GrayF::from_rgb`), `239-259`
  (`NccLevel::score`).
- Fault: the picture is reduced to one luma value for each pixel, and the
  score is the zero-mean normalised correlation. That score is 1.0 for any
  window whose luma is `a * template + b` with `a > 0`. So these all match
  a template at about 1.0: the same button under the dark layer that a
  modal dialog puts over a window, the disabled (dimmed) state of a control,
  and the same shape in another colour where the luma keeps its order, for
  example a red and a green status mark. The module comment says that
  matching is on greyscale, so the first half is known. The second half,
  that a dimmed copy is a full match, is not written down, and the default
  threshold of 0.9 does not separate the cases.
- What follows: `wait_for_image` returns before the control can be used, or
  on the wrong one of two states, and the script goes on.
- Severity: wrong result. A script acts on the wrong screen.
- Proof: two unit tests in `src/vision/matching.rs`.
  `a_dimmed_copy_is_not_a_full_match`: cut a template from `test_screen`,
  then replace every channel value `v` of the screen by `v / 2 + 20`. Assert
  that the score at the place is below the default threshold, or that the
  options have a way to ask for it. Today `find_template` returns the place
  with a score near 1.0. `a_colour_swap_is_not_a_full_match`: a template of
  two colours, (255, 0, 0) on black. A screen with the same shape in
  (0, 130, 0) on black. Today it matches with a score near 1.0.
- Confidence: high. It follows from the formula.

### F7. The OCR scratch file has a predictable name in the shared temp directory

- Where: `src/vision/ocr.rs:195-201` (`ocr`), `207-212` (`write_png`),
  `239-243` (`temp_png_path`).
- Fault: the picture for tesseract goes to
  `<temp dir>/vmlab-ocr-<pid>-<counter>.png`. The counter starts at 0 in
  each process. `File::create` follows a symbolic link, truncates a file
  that exists, and creates the file with the mode that the umask leaves,
  usually 0644. The temp directory is `/tmp` unless `TMPDIR` says another
  place. So: another local user can read the screen of the guest (the crop,
  enlarged) while tesseract runs. Another local user can make the names
  ahead of time. On a host with `fs.protected_symlinks = 0` the daemon then
  writes a PNG over any file its user can write. On a default host the
  create fails and OCR fails for as long as the names exist. The lab's
  private runtime directory (mode 0700) is not used here, though the screen
  capture next to this code uses it (`capture_dir`).
- Severity: security against a local peer (disclosure of the guest's
  screen, and a file overwrite or a denial where the sysctls allow it).
- Proof: test `the_scratch_file_is_private` in `src/vision/ocr.rs`. Put a
  script named `tesseract` first on `PATH` that prints the mode and the
  parent directory of its first argument (`stat -c '%a %h' "$1"`). Run
  `ocr` on a small image. Assert that the mode is 600 and that the
  directory is not the shared temp directory, or that its mode is 700.
  Today the answer is `644 /tmp`. Second test,
  `a_symlink_at_the_scratch_name_is_not_followed`: set `TMPDIR` to a new
  directory, make symbolic links to a victim file at the next 64 scratch
  names, run `ocr`, assert that the victim file is unchanged. Today it
  holds a PNG.
- Confidence: high.

### F8. A cancelled or timed-out OCR leaves tesseract running and its file behind

- Where: `src/vision/ocr.rs:199-201` (the file is removed after the await),
  `214-222` (`run_tesseract`: no `kill_on_drop`, no limit),
  `src/scripting/mod.rs:408-433` (`wait_for_text`),
  `src/scripting/cancel.rs:107-114` (a cancel drops the future),
  `src/vision/ocr.rs:150-155` (a frame above the pixel budget is not made
  smaller).
- Fault, part 1: when the script is cancelled during the OCR, `block_on`
  drops the future. The `remove_file` after the await never runs, and the
  child is not killed, so tesseract runs on and the PNG stays in the temp
  directory. The PNG has up to 16 megapixels, or the full frame where the
  frame is larger. Nothing removes old scratch files at a later start.
- Fault, part 2: `wait_for_text` looks at its deadline only after an OCR
  has returned. Tesseract has no limit of its own. The time of one run
  grows with the area and with how much the picture looks like text, and
  both are the guest's choice: `plan` enlarges a small frame up to
  16.4 megapixels, and passes a larger frame whole, up to the 67 megapixels
  that the display limits allow. `timeout_secs` is exceeded by one full run
  of tesseract.
- Severity: wrong result (a timeout that is not kept), and disk and CPU
  that a guest can make the host spend.
- Proof: test `a_dropped_ocr_kills_tesseract_and_removes_its_file` in
  `src/vision/ocr.rs`, `start_paused = true`. The `tesseract` on `PATH` is
  a script that writes its pid and its first argument to a file and then
  reads from a FIFO that nobody writes. Poll the `ocr` future until that
  file exists, drop the future, then open the FIFO for writing and close
  it. Assert that the scratch path does not exist and that the pid is gone.
  Today both are still there. For part 2, test
  `wait_for_text_keeps_its_timeout_while_tesseract_runs` in
  `src/scripting`: the same script, `timeout_secs` 1. Assert that the call
  returns the timeout error. Today it never returns.
- Confidence: high.

### F9. A large duration panics `Settings::resolve`

- Where: `src/record/mod.rs:207-224` (`Settings::resolve`), `316-354`
  (`parse_duration`).
- Fault: `resolve` accepts every finite positive number of seconds and
  gives it to `Duration::from_secs_f64`, which panics above about 1.8e19.
  `parse_duration` returns such numbers: `1e20` through the bare-number
  branch, and `99999999999999999999h` through the unit branch.
  `compress_idle` has the same code. `resolve` runs in the lab daemon
  (`src/labd/record.rs:133-134`), so the panic is in the handler of
  `machine.record_start` or of a start verb with `--record`, and the wire
  carries the number as given.
- Severity: crash of a request, from the caller's own value.
- Proof: unit test `a_duration_too_large_for_a_clock_is_refused` in
  `src/record/mod.rs`:
  `Settings::resolve(Path::new("a.mp4"), &RecordOptions { max_duration: Some(1e20), ..Default::default() })`
  returns `Err`. Today it panics. The same for `compress_idle`.
- Confidence: high.

### F10. The match threshold is not checked

- Where: `src/vision/matching.rs:102` (`if score < opts.threshold`),
  `src/scripting/mod.rs:547-559` (`match_opts`),
  `src/labd/mod.rs:762-771`, `src/cli/mod.rs:505-507`.
- Fault: no surface checks the threshold. The comparison is false for NaN,
  so with a NaN threshold every search returns its best position, whatever
  the score, including a negative one. A threshold of 0 or below does the
  same. A threshold above 1 can never match and waits for the whole
  timeout. The CLI help says 0.0 to 1.0 and does not enforce it. Over the
  wire a NaN becomes `null` and the request fails with a decode error that
  does not name the threshold.
- Severity: wrong result, from the caller's own value: a match on a screen
  that does not show the image.
- Proof: unit test `a_threshold_outside_zero_to_one_is_refused` in
  `src/vision/matching.rs` (or where the fix puts the check): a template of
  block texture and a screen of an unrelated texture, threshold
  `f64::NAN`, then `-1.0`. Assert no match or an error. Today both return
  `Some`. Assert an error for `1.5`.
- Confidence: high.

### F11. A text match carries the click point (0, 0)

- Where: `src/scripting/mod.rs:414-424`.
- Fault: `wait_for_text` returns the same `ScriptMatch` type as
  `wait_for_image`, with `x`, `y`, `w`, `h`, `cx` and `cy` all 0 and score
  1.0. Tesseract is run for plain text, so no position is known. A script
  that clicks `cx, cy` of the match, as it does after `wait_for_image`,
  clicks the top left corner of the screen, which on most desktops is a
  menu. Nothing tells the two kinds of match apart.
- Severity: wrong result. A script acts on the wrong place.
- Proof: test in `src/scripting`: with a `tesseract` script that prints
  `Continue`, call `wait_for_text("Continue", 1, default)`. Assert that the
  result has no click point (an error on use, or a field that says so).
  Today `cx == 0 && cy == 0 && score == 1.0`.
- Confidence: high for the values. The script API may document it. I did
  not read the manual page.

## Notes

These have no proving test, or they are choices of design.

- N1. A recording is an MP4 with the index at the end (`+faststart`,
  `src/record/encoder.rs:146-147`). When the lab daemon dies or is killed,
  every live MP4 is not playable. WebM survives a cut. The module comment
  of `src/labd/record.rs` says a recording "is always finalised to a
  playable file".
- N2. The timeline keeps every input for the life of the recording, also
  when no timeline was asked for (`src/record/core.rs:364-368`). With
  `timeline` on, the sidecar `<output>.events.json` holds every text that
  vmlab typed (`InputEvent::Text`), so a password that a script types is in
  it, in a file made with the umask's mode.
- N3. The frame work runs on the async workers under a `std::sync::Mutex`:
  `rect` compares and sets each pixel through `get_pixel` and `put_pixel`,
  and `base` resizes the whole source for each changed frame
  (`src/record/core.rs:259-279`, `487-512`). `find_template` runs in the
  request handler without `spawn_blocking`
  (`src/labd/display/mod.rs:319-333`). I did not measure what else waits on
  those workers.
- N4. The cost of one image search grows with the guest's screen. For each
  of the 8 candidates `scan_full` builds `NccLevel::new`, which makes two
  integral images of the whole screen (`src/vision/matching.rs:351-355`,
  `181-198`): 16 bytes a pixel, 8 times in a row, next to 8 bytes a pixel
  for the grey copy. At the 67 megapixels that the display limits allow
  this is about 1.6 GB at the peak in the lab daemon. A template below 8
  pixels on one side is searched without the pyramid, at
  `W * H * w * h` steps.
- N5. `record_launch` holds the lock of the live recordings while
  `Recorder::start` connects (`src/labd/record.rs:173-195`). A display
  server that accepts and does not answer holds every record verb and
  `record_finalise_all` for that time. I did not read the timeouts of
  `VncClient::connect`.
- N6. A recording that ended by itself (the maximum duration, or ffmpeg
  died) emits `record.stopped` only when somebody asks for the status or
  stops it (`src/labd/record.rs:310-322`). Nobody is told that ffmpeg died
  in the middle of a run.
- N7. `EncoderPlan::new` puts the output path into the arguments with
  `display()` (`src/record/encoder.rs:175`). A path that is not UTF-8 is
  changed, ffmpeg writes another file, and `Finished.path` names the
  original one. ffmpeg runs with `-y`, so a file at the path is replaced
  without a question.
- N8. A recording without `max_duration` has no size limit of its own. It
  ends when the disk watchdog trips.
- N9. A region with width or height 0 is accepted by `ocr::plan`
  (`src/vision/ocr.rs:136-144`): the crop is empty, the scaled size is
  1 x 1, and tesseract reads a black pixel. The result is an empty text and
  no error, so `wait_for_text` waits for its whole timeout. Negative
  numbers become 0 on the way (`src/scripting/ocr.rs:38`), which
  `docs/reviews/wscript.md` already names. `find_template` returns `None`
  for a region outside the screen, where `ocr` returns an error.
- N10. A template of one flat colour is scored by the mean absolute
  difference (`src/vision/matching.rs:264-272`). At the default threshold
  it matches any area within 25 grey levels, and the first such place in
  scan order wins. Two exact occurrences of any template give the first in
  scan order, and nothing says that there were two.
- N11. A match is found on one capture and the click is scaled by a later
  capture (`size()`). A guest that changes its mode between the two moves
  the click. Each pointer action costs one full capture for the size alone.
- N12. In a debug build, a ripple or a pointer at a coordinate near
  `i64::MAX` overflows in `draw_ripple` and `draw_cursor`
  (`src/record/core.rs:613`, `650`). A release build wraps and the pixel is
  outside the frame, so nothing is drawn. The coordinates come from the
  script.
- N13. After a size change the old picture stays until new pixels come, but
  ripples and the cursor are placed with the new size
  (`src/record/core.rs:236-246`, `518-535`). Cosmetic.
- N14. `run_tesseract` drops stderr (`src/vision/ocr.rs:220`), so a failure
  says only `tesseract failed with exit status 1`. Cosmetic.

## Other work seen

- `docs/reviews/display.md` F1 and F2 (sizes from the display server) are
  fixed in this commit: both clients limit the screen and the cursor. F1 of
  this document is what those limits leave open at the low end.

# Debugging notes: OV02C10 / Intel IPU6 on Debian

This documents the investigation behind this repo, for anyone hitting the
same wall on similar hardware (Dell XPS 16, OV02C10 sensor, Intel IPU6
image processor, mainline/Debian kernel — likely applicable to other
IPU6-based laptops too).

> These notes were written by Seth Barrett against the original Python
> implementation (numpy debayer, GStreamer `appsrc`/`v4l2sink`). The tool is
> now a single Rust binary with no Python, numpy or GStreamer, so the
> PyGObject/`uv` and GStreamer buffer items below are history. Everything
> about the hardware (pipeline order, pixel format, CSI2 quirks, gain) still
> applies and is what the Rust code implements.

## The three candidate approaches, and why two are dead ends

1. **`libcamera` (via `cam`, `pipewire-libcamera`, etc.)** — sees the
   camera and can capture, but only through its `uncalibrated.yaml`
   fallback IPA, since no `ov02c10.yaml` tuning file has been upstreamed
   for this sensor. Symptom: `cam --list-controls` and captures work, but
   there is no real AGC (auto gain control) loop — frame 1 after stream
   start carries a valid image, every frame after it goes solid black.
   Root cause: `IPASoft: Failed to create camera sensor helper for
   ov02c10` — libcamera's software ISP has no registered
   `CameraSensorHelper` for this sensor model, so its blind AGC can't
   correctly convert between register gain values and real-world units,
   and drives exposure/gain to the floor. Fixing this properly means
   patching and rebuilding libcamera itself to add a sensor helper —
   possible, but a real C++ change, not a config tweak.

2. **Intel's proprietary HAL (`icamerasrc` / `libcamhal`, GStreamer)** —
   if it's already installed (`libcamhal-ipu6epmtl` /
   `gstreamer1.0-icamera`, likely pulled in as an Ubuntu OEM `.deb`, since
   Debian's own repos don't ship it), `gst-inspect-1.0 icamerasrc` will
   correctly detect the sensor by name (`ov02c10-uf`) and it *does* ship
   real per-module `.aiqb` tuning data under `/etc/camera/ipu6epmtl/`.
   It still fails, though:
   ```
   CamHAL[ERR] Failed to open PSYS, error: No such file or directory
   ```
   PSYS is the IPU6's hardware image processor (does debayer/3A in
   silicon). Check what's actually loaded:
   ```bash
   lsmod | grep -i ipu6
   find /lib/modules/"$(uname -r)" -iname "*ipu6*"
   ```
   On mainline/Debian kernels you'll typically find `intel-ipu6.ko` and
   `intel-ipu6-isys.ko` (capture-only) but **no `intel-ipu6-psys.ko`** —
   that driver only ships in Ubuntu's OEM-patched kernels or Intel's
   out-of-tree `ipu6-drivers` DKMS package. Getting this path working
   means building and maintaining an out-of-tree kernel module against
   your exact kernel version — real ongoing risk, breaks on every kernel
   update. Not worth it unless you have no other option.

3. **Raw V4L2 capture + software debayer (what this repo does)** — bypass
   both of the above. Open the ISYS capture node directly, pull raw
   packed-10-bit Bayer frames, debayer and gain-correct them in Python
   with numpy, and feed the result into a GStreamer `appsrc` pipeline.
   No PSYS, no libcamera IPA, no proprietary HAL. Slower and cruder than
   real ISP hardware, but it's the only path that doesn't depend on
   kernel modules you don't have.

## Bugs found and fixed in the raw-capture path

- **Stale hardcoded I2C bus number.** The sensor's media-controller entity
  name embeds its I2C bus (e.g. `ov02c10 5-0036`), and that bus number is
  **not stable across boots** — ACPI enumerates I2C devices in a
  different order depending on boot state. A hardcoded bus number will
  silently go stale (all `media-ctl` calls referencing it fail, but with
  `check=False` logging they don't loudly break anything). Resolve it
  live instead:
  ```bash
  media-ctl -d /dev/media0 -p | grep -i ov02c10
  ```
  `find_sensor_entity()` in `camera/ov02c10_camera/media_pipeline.py` does this automatically at
  every startup instead of hardcoding a bus number.

- **No exposure/gain control at all.** There's no AE/AGC loop anywhere in
  the raw V4L2 path — the sensor sits at whatever its driver's power-on
  defaults are. On this hardware that turned out to be `exposure` pinned
  at its **maximum** (2320/2320) with `analogue_gain` barely off the
  floor (20/248) and `digital_gain` at rock bottom (1024/16383) — i.e.
  a very dark raw signal with no compensating gain, even in a well-lit
  room. Check current values with:
  ```bash
  v4l2-ctl -d "$(media-ctl -d /dev/media0 -e 'ov02c10 5-0036')" -l
  ```
  `set_sensor_gain()` sets `analogue_gain`/`digital_gain` explicitly via
  `VIDIOC_S_CTRL` on the sensor subdevice before streaming starts. The
  defaults in this repo (`analogue_gain=150`, `digital_gain=4096`) were
  tuned for one indoor room and are a fixed value, not real auto-exposure
  — retune via `--analogue-gain`/`--digital-gain` if your lighting is
  very different.

- **CPU/memory blowup in the debayer step.** The original debayer
  upsampled half-resolution R/G/B channels all the way back up to *full*
  sensor resolution with `np.repeat` (six large temporary array
  allocations per frame, ~30MB/frame at 30fps) and then immediately threw
  most of it away downsampling to the actual output resolution. This
  pegged a CPU core and, combined with sustained large-block allocation
  churn, drove RSS into the tens of GB within seconds via allocator arena
  growth. Fixed by gathering directly from half-res to output resolution
  with a single indexed copy per channel — no full-resolution
  intermediate at all.

- **Per-frame `Gst.Buffer` leak.** `Gst.Buffer.new_wrapped(bytes(view))`
  called every frame ties a fresh Python `bytes` object to a new
  `GstBuffer` 30 times a second; in practice it was never being reclaimed.
  Switched to `Gst.Buffer.new_allocate()` + `.fill()`, which allocates
  from GStreamer's own memory pool and copies into it instead.

- **`free_device()` was killing the system's PipeWire multimedia service.**
  Every `make run` broke live audio/mic routing (discovered mid-Teams-call).
  The original implementation ran `fuser -k <device>` unconditionally to
  clear the capture node before opening it — but `fuser` reported
  `pipewire`/`wireplumber`'s PIDs as holding `/dev/video32` (WirePlumber
  keeps a brief monitoring/enumeration handle on camera devices as part of
  normal desktop media-session management, not an exclusive streaming
  lock), and `fuser -k` killed them along with everything else, taking the
  whole system's audio down with it. Fixed by checking each held-device
  PID's process name via `ps -o comm=` first and never killing
  `pipewire`/`wireplumber`/`pipewire-media-session` — only genuinely
  conflicting processes get killed now.

## Environment setup issues

- **Debian's packaged `v4l2loopback-dkms` can fail to build on newer
  kernels.** Trixie ships `v4l2loopback-dkms 0.15.0`, which fails against
  kernel `6.17.13` with:
  ```
  error: implicit declaration of function 'setup_timer'
  ```
  `setup_timer()` was removed from the kernel timer API; upstream
  `v4l2loopback` added a `#if defined(timer_setup)` compatibility branch
  to handle this, but that fix landed after the `0.15.0` release Debian
  packaged. Confirmed fixed as of upstream tag `v0.15.4`. `scripts/setup.sh`
  builds that version from source via DKMS instead of relying on the apt
  package, so it still auto-rebuilds on kernel upgrades like a normal DKMS
  module.

- **`ModuleNotFoundError: No module named 'gi'` when running via `uv run`/the
  systemd service, even though `python3 -c "import gi"` works fine.**
  PyGObject is installed via apt against the *system* Python
  (`/usr/lib/python3/dist-packages`) — it's a C-extension GObject
  Introspection binding, not something pip can build. `uv sync`/`uv venv`
  by default download and manage their own standalone CPython build
  (e.g. `~/.local/share/uv/python/cpython-3.14-...`), a completely
  separate interpreter installation. Setting `include-system-site-packages
  = true` in `pyvenv.cfg` doesn't help — it only adds *that interpreter's*
  site-packages dir, and uv's standalone build has nothing installed via
  apt. Fix: pin the venv to the actual system interpreter so
  system-site-packages correctly points at the dist-packages `gi` is
  actually in:
  ```bash
  uv venv --python /usr/bin/python3 --system-site-packages
  uv sync --dev
  ```
  `Makefile`'s `sync`/`install` targets and `scripts/setup.sh` do this
  automatically now — `uv sync` alone (without a pre-existing correctly
  configured `.venv`) will silently create a venv that can't see `gi`.

- **`/dev/video48` gets created root-only.** Manually `modprobe`-ing
  `v4l2loopback` without the distro package's udev rule leaves the device
  node as `crw------- root root` — neither a `systemd --user` service nor
  a browser running as a regular user can open it. Fixed by installing a
  udev rule (`GROUP="video", MODE="0660"`) and ensuring the user is in the
  `video` group — both handled by `scripts/setup.sh`. Group membership
  changes require logging out and back in to take effect.

## Design decisions

- **On-demand activation uses a kernel event, not polling.** #10 asked for
  the camera to only run while something is using `/dev/video48`. The
  first version polled `fuser /dev/video48` from a separate watcher
  service; that could not work with browsers (next section). The current
  version subscribes to v4l2loopback's `V4L2_EVENT_PRI_CLIENT_USAGE` event
  on the producer's own file descriptor. The module raises it whenever a
  reader starts or stops streaming, so there is nothing to poll, no second
  service, and no need to tell the producer's own open apart from a
  consumer's. This is the same mechanism Intel's `v4l2-relayd` uses.
- **One process, no GStreamer.** The process that owns the loopback device
  also owns the sensor. Frames are converted to YUYV and handed to the
  loopback device with plain `write()`.

## On-demand activation with Chrome/Brave (#10): how it was fixed

The original watcher design could not work with browsers, because of how
`v4l2loopback`'s `exclusive_caps=1` mode behaves:

- With `exclusive_caps=1`, the device's reported V4L2 capability is *live*:
  it announces **OUTPUT-only** while no producer is attached, and only
  announces **CAPTURE** (what browsers filter on when listing cameras)
  while a producer is streaming into it. In v4l2loopback 0.15 "streaming"
  means the producer has actually written a frame; setting the output
  format alone is not enough, and the capability reverts as soon as the
  producer closes the device.
- Chrome/Brave only list devices that currently report CAPTURE, so with
  nothing producing, the camera never appeared in the picker, and nothing
  could ever open the device to wake the watcher.

The fix is the placeholder producer the original notes suggested, built
into the tool itself (`ov02c10-camera --on-demand`):

1. At startup it opens `/dev/video48`, sets the output format and writes
   one black frame. The device now reports CAPTURE and shows up in every
   camera picker. The sensor is still off, and so is its LED.
2. It waits on the client-usage event. While idle it rewrites the black
   frame about once a second, which costs effectively nothing.
3. When an application starts streaming, the event fires, the sensor is
   started, and real frames replace the black one (after a short delay:
   pipeline setup plus exposure and white-balance calibration).
4. When the last reader stops, the sensor is shut down after
   `--idle-secs` (default 5), keeping the last gain for the next start.

If the loaded v4l2loopback is too old to support the event, the tool logs
a warning and runs the sensor continuously instead.

## Useful diagnostic commands

```bash
# Find the sensor's current media-controller entity (bus number can change per boot)
media-ctl -d /dev/media0 -p | grep -i ov02c10

# Current sensor control values (exposure, gain, etc.)
v4l2-ctl -d "$(media-ctl -d /dev/media0 -e 'ov02c10 <BUS>-0036')" -l

# Set gain manually for a quick test
v4l2-ctl -d "$(media-ctl -d /dev/media0 -e 'ov02c10 <BUS>-0036')" -c analogue_gain=150,digital_gain=4096

# Confirm whether PSYS is available at all before attempting the proprietary HAL route
lsmod | grep -i ipu6
find /lib/modules/"$(uname -r)" -iname "*ipu6*psys*"
```

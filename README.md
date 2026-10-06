# ov02c10-camera-fix

Working camera capture for laptops with an **OV02C10** sensor behind an
**Intel IPU6** image processor (e.g. Dell XPS 16) on Debian/mainline-kernel
systems, where neither `libcamera` nor Intel's proprietary camera HAL
produce a usable image out of the box.

Feeds a corrected image into a [v4l2loopback](https://github.com/v4l2loopback/v4l2loopback)
virtual camera device, so it shows up as a normal webcam in Brave, Chrome,
Teams, Zoom, Discord, etc.

This is a Rust rewrite of [Seth Barrett's original Python
implementation](https://github.com/sethbarrett50/ov02c10-camera-fix). The
hardware investigation and the capture recipe are his; see
[`docs/DEBUGGING.md`](docs/DEBUGGING.md). He also tested every release of this
version on his own laptop.

## What changed in the rewrite

- **One static binary**, no Python, numpy or GStreamer. The
  only runtime dependency is `v4l-utils` (`media-ctl`, `v4l2-ctl`).
- **On-demand activation works with Chrome/Brave.** The sensor (and its
  LED) is only on while an application is using the camera, and the camera
  still shows up in the browser's picker while idle. One systemd unit
  instead of a service plus a polling watcher.
- **Continuous auto-exposure.** Analogue gain follows the room lighting
  while streaming, not only at startup.
- **A `.deb`** built by CI for every change.

Not carried over: the preview window. Use `--snapshot FILE` for a quick
look at what the sensor sees, or open the camera in any application.

## Why this exists

- `libcamera`'s software fallback IPA (`uncalibrated.yaml`) captures a
  valid first frame, then every frame after it comes out solid black.
- Intel's proprietary HAL (`icamerasrc`/`libcamhal`) fails with
  `Failed to open PSYS`: the IPU6's hardware ISP driver isn't in
  mainline/Debian kernels.

This tool instead captures raw Bayer frames directly via V4L2, debayers,
white-balances and exposes them in software, and writes the result to a
v4l2loopback device.

## Install

Requirements: the OV02C10 sensor behind Intel IPU6 (`lsmod | grep ipu6`
shows `intel_ipu6` and `intel_ipu6_isys`), and the `v4l2loopback` kernel
module.

```bash
# 1. The v4l2loopback kernel module, if you don't have it yet
#    (check: modinfo v4l2loopback)
sudo apt install v4l2loopback-dkms
#    If that fails to build on your kernel, clone this repo and run
#    ./scripts/setup.sh, which builds 0.15.4 from source (the version the
#    on-demand mode was written against).

# 2. The package (latest build of the dev branch)
wget https://github.com/SamuelSchwertfeger/ov02c10-camera-fix/releases/download/dev-latest/ov02c10-camera_amd64.deb
sudo apt install ./ov02c10-camera_amd64.deb
```

That is all: installing enables the service for every user and starts it for
users logged in now (an upgrade restarts it with the new binary). Open a
camera test page or a call: "OV02C10 Camera" appears in the picker, and the
sensor and LED only turn on while an application is streaming from it.

To see how it detects applications using the camera:
`journalctl --user -u ov02c10-camera -b | grep -i "reader events"`. A match
means the loaded v4l2loopback lacks reader events and readers are found by
open handles instead. In that mode any application that keeps the camera
open counts, even when it is not streaming, and apps reaching the camera
through PipeWire are not detected. To get events, replace the module with a
newer one: `sudo apt remove v4l2loopback-dkms`, then `./scripts/setup.sh`
(it only builds when no v4l2loopback module is installed), and reboot.

The package installs the binary, the systemd `--user` unit, a udev rule
giving the logged-in user access to the camera nodes, and a modprobe
configuration that loads `v4l2loopback` at boot as `/dev/video48`.
Tagged releases are on the [releases page](../../releases).

Things to know:

- If `v4l2loopback` was already loaded with other options, reload it so
  `/dev/video48` exists: `sudo modprobe -r v4l2loopback && sudo modprobe v4l2loopback`.
- The picture is turned 180 degrees, because the sensor is mounted upside
  down and current kernels no longer flip it. If yours comes out upside
  down, your kernel still flips it: add `--no-rotate` to `ExecStart` with
  `systemctl --user edit --full ov02c10-camera`.
- If you previously ran the Python version's `make install`, remove its
  units first, since they take precedence over the packaged one:
  ```bash
  systemctl --user disable --now ov02c10-camera-watcher ov02c10-camera
  rm -rf ~/.config/systemd/user/ov02c10-camera*
  systemctl --user daemon-reload
  ```

### From source

```bash
make setup     # v4l-utils, cargo, v4l2loopback (needs sudo)
make install   # build, package, install (the package enables and starts the service)
```

Needs Rust 1.85 or newer (Debian 13's `cargo` is enough).

## Usage

```
ov02c10-camera --on-demand      # what the service runs
ov02c10-camera --loopback       # sensor always on
ov02c10-camera --snapshot f.ppm # one frame to a file, then exit
ov02c10-camera --help           # all options
```

`make logs` tails the service's journal; add `-v` to a foreground run for
every `media-ctl`/`v4l2-ctl` call.

When started, the tool kills other processes holding the raw capture node
(`/dev/video32`), such as a stale instance or a stray `cam`. PipeWire and
WirePlumber are left alone.

## Tuning

Auto-exposure adjusts the sensor's analogue gain (16 to 248) to keep the
average brightness near `--ae-target` (default 128), and backs off when
highlights clip. White balance is measured once per sensor start.

```bash
make gain                                              # current sensor controls
ov02c10-camera --loopback --ae-target 110              # darker image
ov02c10-camera --loopback --no-auto-exposure --analogue-gain 100
```

To change the service's options, `systemctl --user edit ov02c10-camera`
and override `ExecStart`.

## Development

```
src/
  main.rs      entry point, logging
  config.rs    command line
  image.rs     unpack, statistics, white balance, debayer, YUYV
  exposure.rs  auto-exposure
  media.rs     media-ctl / v4l2-ctl wiring (sensor discovery, gain, links)
  v4l2.rs      V4L2 structs and ioctls, capture and loopback devices
  run.rs       run modes, stream loop
```

`make check` and `make test` are what CI runs. The tests cover the
hardware-independent logic (image math, auto-exposure, argument and
`media-ctl` parsing, ioctl numbers); the V4L2 path can only be tested on
real hardware. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT, see [LICENSE](LICENSE). Original work copyright Seth Barrett.

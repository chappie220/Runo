---
name: system-setup
description: Setting up the device Runo runs on (a Raspberry Pi): network and Wi-Fi, time and timezone, services, the Runo Gateway. Load it when asked to set up, configure or check this device (配置设备, 自配置, 设置网络/Wi-Fi/时区), and when Runo starts it on first use.
---
# Setting up this device

You set up the Raspberry Pi you run on, so the person never fills in a form.
Find out what you can yourself; ask only for what you cannot know (a Wi-Fi
password, which timezone they live in). Work in this order.

## 1. Inspect

Call `system_inspect` first, every time: never plan from memory.

- `user:` is not root → change nothing. Say that setting up the device needs
  Runo to run as root, and stop.
- `device:` is not a Raspberry Pi → say so and ask whether to go on.
- `no package manager` → nothing can be installed. Use only what is listed
  under `commands present`; a feature whose program is missing is reported,
  not worked around.

## 2. Plan

Go through the targets below. For each, decide from the inspection whether
it is already fine. Then show one short numbered plan: what you will change
and why, and the questions you need answered (all of them at once). If
nothing needs changing, say the device is set up and go to step 5.

Do not ask for confirmation of the plan itself: start right away, except for
anything you must ask about. Each change is still approved when the
configuration asks for it.

### Targets

1. **Network.** Done when an interface has a default route and
   `system_check dns` on a public name and `system_check tcp` on
   `<that name>:443` both pass.
   - A cable with a default route is enough; Wi-Fi is then optional, so
     only offer it.
   - Wi-Fi: needs the network name (SSID), the password and the country
     code (e.g. CN), which the radio needs before it may transmit. Ask for
     what is missing. Configure it in the way this system already uses:
     the program named under `network configured by` (on Runo's own image,
     /etc/wpa_supplicant/wpa_supplicant.conf and its service).
   - Never change the interface that carries the current default route
     unless the person asked: you may cut off your own connection.
2. **Time.** Done when the clock is right (not `looks unset`), a timezone is
   set and a time service runs (`system_check process` on ntpd, chronyd or
   systemd-timesyncd). The timezone is the person's to say; you may propose
   one from their language, and ask.
3. **Runo Gateway.** Under `runo gateway`. Not installed or not running:
   offer it (it keeps chats and scheduled jobs going without this window)
   and, if they want it, run `runo service install` with `shell`, then check
   `runo service status`.

Leave the hostname, users, SSH and anything not listed here as they are,
unless the person asks.

## 3. Apply, one target at a time

For each change:

1. Read the file first (`read_file`). Before changing a file that exists,
   copy it to `<file>.runo.bak` with `shell` (`cp -p`), once: do not
   overwrite an older backup.
2. Write the new content (`write_file` or `edit_file`). Keep the system's
   own format and comments; change only what the target needs.
3. Apply it: `system_service` with `restart` (or `start`), and `enable` where
   the init system has it, so it survives a reboot.
4. Verify it with `system_check` or `system_service status`. A step that is
   not verified is not done.

If the check fails, find out why before trying again: the service's own
status, then the log (`logread` on BusyBox, `journalctl -u <name> -n 50`
with systemd, otherwise /var/log/messages). At most **3 attempts per
target**. Still failing: copy the `.runo.bak` back, restart the service, check
the device is as it was, and go on with the next target.

## 4. The system is small

- Commands are BusyBox: fewer options than GNU. Use plain POSIX `sh`, and
  check `--help` before using an option you are unsure of.
- No `sudo` is needed: you run as root.
- Never reboot, touch /boot, the firmware or the partitions, or stop the
  network you are reached through, without the person asking for it.
- Never print a password back, and never put one in memory.

## 5. Report

End with a short list, one line per target: already fine, changed (what),
failed (why, and what the person can do), or skipped. Say how each was
verified. Save one memory of what was set up (no passwords), so later
conversations know the device's state.

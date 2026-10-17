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
  not worked around. Runo's own image has apk: a missing program can be
  installed from the Alpine repositories (`apk add`), with the person's
  approval.

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
     the program named under `network configured by`. On Runo's own image
     (`runo system:` in the inspection) that is /etc/wpa_supplicant.conf,
     applied by restarting `runo-network`.
   - Never change the interface that carries the current default route
     unless the person asked: you may cut off your own connection.
2. **Time.** Done when the clock is right (not `looks unset`), a timezone is
   set and a time service runs (`system_check process` on ntpd, chronyd or
   systemd-timesyncd). The timezone is the person's to say; you may propose
   one from their language, and ask. On Runo's own image it is the zone name
   in /etc/timezone, applied by restarting `runo-time`.
3. **Runo Gateway.** Under `runo gateway`. Not installed or not running:
   offer it (it keeps chats and scheduled jobs going without this window)
   and, if they want it, run `runo service install` with `shell`, then check
   `runo service status`.

4. **Remote access from outside (optional).** Only when the person asks to
   reach the device away from home. It needs a public IPv6 address (the
   "Public access" line of the status says when the network gives none:
   tell them so plainly — there is no IPv4 fallback) and a domain of
   theirs at Cloudflare, Alibaba Cloud DNS or Tencent DNSPod. Never ask for
   the provider's credentials in the conversation: tell them to type `!shell`
   and run `runo daemon ddns set <cloudflare|aliyun|dnspod> <domain>`, which
   asks for them without echo; `runo daemon ddns` then shows the result.
   Clients outside the network also need the device password, and the
   router must let IPv6 in to port 7340.

Leave the hostname, users, SSH and anything not listed here as they are,
unless the person asks.

## 3. Apply, one target at a time

Every change to the system goes through `system_change`: never write files
under /etc with `write_file`, `edit_file` or `shell`. One call is one
target, named in `target` (`network`, `time`, `gateway`):

1. Read the files first (`read_file`). Keep the system's own format and
   comments; change only what the target needs, and give each file whole.
2. Name the services to restart, and the checks that show the target works
   (`dns`, `tcp`, `http`, `process`, `service`). A network change needs
   `settle_secs` long enough for the link to come back (about 30).
3. `system_change` saves the old files, writes, restarts and checks. When a
   check fails it puts everything back by itself and says so; the device is
   then as it was.
4. For a service that should start at boot, `system_service enable` after
   the change is applied (where the init system has it).

When a change was put back, find out why before trying again: the
service's own status, then the log (`logread` on BusyBox,
`journalctl -u <name> -n 50` with systemd, otherwise /var/log/messages).
A target that was put back 3 times is refused for the rest of the
set-up: leave it, go on with the next one, and say why in the report.

`system_history` lists what was changed; `system_revert` undoes a change
when the person asks for it.

## 4. The system is small

- Commands are BusyBox: fewer options than GNU. Use plain POSIX `sh`, and
  check `--help` before using an option you are unsure of.
- No `sudo` is needed: you run as root.
- Never reboot, touch /boot, the firmware or the partitions, or stop the
  network you are reached through, without the person asking for it.
- Never print a password back, and never put one in memory.

## 5. Limits

The whole set-up, over all its turns, may make 50 tool calls and spend
$1 by default (`limits.setup_tool_calls`, `limits.setup_usd`). When one is reached it
stops by itself; what was already verified stays. So inspect once, do not
check the same thing twice, and ask all your questions together.

## 6. Report

Call `system_setup_done` first. Then end with a short list, one line per
target: already fine, changed (what, with its change number), failed (why,
and what the person can do), or skipped. Say how each was verified. Save
one memory of what was set up (no passwords), so later conversations know
the device's state.

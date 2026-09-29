# Design: the hourly agent on Linux and Windows

Status: proposal, with product decisions 1, 5, 6, and 7 answered on 2026-09-29. This document builds
nothing and changes no behavior.

The release builds five platforms. Only macOS gets the hourly agent: `link` writes a LaunchAgent
there. On Linux and on Windows, the content updates only when a Claude session starts, and the binary
updates only when a person runs `self-update`. [README.md](../../README.md), [API.md](../API.md), and
[ARCHITECTURE.md](../ARCHITECTURE.md) say so. [SECURITY.md](../SECURITY.md) names the cost: an update
that does not reach the copy that the hook runs leaves every session on the old version, and a fixed
vulnerability stays open. A fleet with mixed systems has two update speeds.

[`src/schedule.rs`](../../src/schedule.rs) already has the shape that a second and a third backend
need. The open questions are about the platforms, and not about the code. This document answers them
in writing, so that the build that follows is a plan and not a guess.

The prototype in `src/schedule.rs` is small. Two functions, `render_systemd` and `render_task`, write
the text of the agent files, and six tests check that text. Only the tests call the functions.
Nothing registers, enables, starts, or loads anything.

## How to read the answers

Each answer names its source as a public URL. **Not verified** marks an answer that no public source
confirms. Such an answer says what would verify it. The section
[Open questions for the maintainer](#open-questions-for-the-maintainer) lists every one, each with an
identifier such as L-3 or W-5.

The limits of this work:

- The systemd manual pages come from two mirrors, `man7.org` and `manpages.debian.org`. The
  `freedesktop.org` site refused the tool that read the pages (HTTP status 403). The reading tool
  cut off `systemd.exec(5)` and `systemd.unit(5)` before the sections on `StandardOutput=` and on
  specifiers. The `NEWS` file in the systemd repository gives the version in which a feature arrived.
- The Windows pages come from Microsoft Learn. Some come from archived pages, and one comes from a
  forum thread that Microsoft hosts. The answer says so when it uses one.
- Other sources are the Rust standard library and `docs.rs`, and the issue tracker of the Windows
  Terminal project.
- No Linux system and no Windows system was available. Neither systemd, `schtasks.exe`, nor Task
  Scheduler has read any file in this document. `xmllint` on macOS checked that the task text is
  well-formed XML in UTF-16. A scratch test checked that the two quoting layers of the Linux unit
  agree with each other (see L-1).
- No command that registers, enables, starts, or loads a timer, a task, or an agent ran.

## What stays fixed

Nine decisions are already made. They are in [ARCHITECTURE.md](../ARCHITECTURE.md), section "An
hourly LaunchAgent installs signed builds without a person". This design keeps every one of them.

| # | Decision | Linux | Windows |
|---|---|---|---|
| 1 | `self-update`, then `sync`, joined so that a failed update never holds the content back | `/bin/sh -c` script, joined by `;` | `cmd.exe` script, joined by `&` |
| 2 | Minute 0 of every hour, and once at each login | `OnCalendar=`, `OnStartupSec=` | `TimeTrigger` that repeats, `LogonTrigger` |
| 3 | Runs as the user, never as root or as a system service | user manager, `systemctl --user` | task of the user, least privilege |
| 4 | Runs the program copy under the root, the file that the hook runs | same prefix as the hook | same file |
| 5 | `link` writes it. `unlink` and `uninstall` remove it before the program copy goes | same order | same order |
| 6 | With `--claude-dir`, no agent, unless `--agent-dir` names a folder. That agent never loads | same rule | same rule |
| 7 | A failure to load is a notice, not an error | notice | notice |
| 8 | The output goes to `<root>/agent.log` | redirect in the script | redirect in the script |
| 9 | The agent passes no `--force` | same | same |

One difference in decision 2 needs a note. macOS starts the agent again at each GUI login. The
systemd user manager starts at the first session of the user, so a second session of the same user
starts no new run. The hourly run covers that case.

The run lock must land first. Every new trigger for `sync` is one more chance for two runs to start
together. `sync` holds an exclusive lock on `<root>/.lock` while it installs. It waits up to 30
seconds for another run, then keeps the installed content and exits 0 with a notice. `self-update`
holds `<root>/.update.lock`, and it exits 1 when it cannot get the lock within 30 seconds. No agent
ships on Linux or on Windows before that code is in the tree.

## The three agents side by side

| | macOS (built) | Linux | Windows |
|---|---|---|---|
| Mechanism | LaunchAgent property list | systemd user timer, two unit files | scheduled task, made from an XML file |
| Name | `it.damac.brainmaker` | `brainmaker.service`, `brainmaker.timer` | `brainmaker-<user>` |
| Every hour | `StartCalendarInterval`, minute 0 | `OnCalendar=*-*-* *:00:00` | `TimeTrigger`, `Interval` of `PT1H` |
| At login | `RunAtLoad` | `OnStartupSec=1min` | `LogonTrigger`, `Delay` of `PT1M` |
| Command | `/bin/sh -c` | `/bin/sh -c` | `cmd.exe /d /v:off /s /c` |
| Log | `StandardOutPath` | `exec >> log 2>&1` | `>> log 2>&1` |
| Load | `launchctl bootstrap` | `systemctl --user enable`, `restart` | `schtasks /Create /XML ... /F` |
| Unload | `launchctl bootout` | `systemctl --user disable --now`, `stop` | `schtasks /End`, `/Delete` |
| Runs while | the user has a GUI session | the user has a login session | the user is logged on |
| `--agent-dir` names | the folder of the property list | the folder of both unit files | the folder of the task XML file |

## Linux

### 1. Mechanism

**Answer.** The agent is a systemd user timer. `link` writes two files, `brainmaker.service` and
`brainmaker.timer`, in `$XDG_CONFIG_HOME/systemd/user`. When that variable is not set, the folder is
`~/.config/systemd/user`. The timer starts the service that has the same name, because the default
of `Unit=` is that name.

Source: the table "Unit File Load Path" in <https://man7.org/linux/man-pages/man5/systemd.unit.5.html>.
The default of `Unit=` is in <https://man7.org/linux/man-pages/man5/systemd.timer.5.html>.

**The alternative is one line in the crontab of the user.** It is worse here for two reasons.

1. A user has one crontab, and the tools replace it as a whole. `crontab -e` installs the edited file,
   and `crontab -` installs a new file. `link` and `unlink` would edit a file that the user also
   edits. Source: <https://manpages.debian.org/testing/cron/crontab.1.en.html>.
2. Cron has no trigger for a login. `@reboot` runs once, when the cron daemon starts. Source:
   <https://manpages.debian.org/testing/cron/crontab.5.en.html>.

### 2. Schedule

**Answer.** The timer holds three settings.

- **Every hour.** `OnCalendar=*-*-* *:00:00` fires at minute 0 of every hour. `hourly` is the
  documented short name for the same value. `systemd-analyze calendar '*-*-* *:00:00'` prints the
  next time. Source: <https://man7.org/linux/man-pages/man7/systemd.time.7.html> and
  <https://man7.org/linux/man-pages/man1/systemd-analyze.1.html>.
- **Accuracy.** The default of `AccuracySec=` is 1 minute. systemd can start the run anywhere in a
  window of that length after the time. The timer sets `AccuracySec=1s`, so the run starts in
  minute 0, as decision 2 says. Source: <https://man7.org/linux/man-pages/man5/systemd.timer.5.html>.
- **At login.** `OnStartupSec=1min` counts from the start of the service manager. The manager of a
  user starts at the first login of that user, and not at boot. `pam_systemd` starts it when the
  first session opens. Source: the same timer page, and
  <https://man7.org/linux/man-pages/man8/pam_systemd.8.html>. The delay of one minute gives the
  network time to come up. macOS has no such delay.

**A run that the machine missed.** `Persistent=true` starts the service when the timer starts, if a
run was due while the timer was off. It works only with `OnCalendar=`. Decision 2 does not want it,
for three reasons.

- The login run already covers an hour that a powered-off machine missed.
- A timer with both settings would start two runs at each login.
- A machine that sleeps through minute 0 needs no setting. A calendar timer that fires during sleep
  runs when the machine resumes.

Source for `Persistent=` and for sleep: the timer page above.

Not verified (L-8). Whether a timer that starts after the delay of `OnStartupSec=` has passed fires at
once. This matters at the first `link`. To verify: run `systemctl --user enable --now brainmaker.timer`
in a session that is older than one minute, and read `systemctl --user list-timers`.

### 3. Command

**Answer.** `ExecStart=` starts a program directly. The manual says that redirection, pipes, and
background programs are not supported. Two options join the two programs.

**Option A: two `ExecStart=` lines.**

- `Type=oneshot` allows several `ExecStart=` lines. They run in order. When one fails, the rest do not
  run, and the unit fails.
- The prefix `-` changes that. systemd records the failure of the command and treats it as success.
  A `-` on the first line gives decision 1.
- The quoting fails. The syntax manual allows an opening quote only at the start of an item, so
  `-"/a b/x"` is not valid. A path with a space needs the escape `\s` in place of quotes.
- systemd quotes differently from a shell. The `prefix` of `link::command_prefix` is shell quoting,
  and the unit would need the raw paths.
- `date` needs a third line.

**Option B: `ExecStart=/bin/sh -c "<script>"`.**

- The script is the script of the macOS agent, with a redirect in front. The same `prefix` works,
  and `;` joins the commands, so a failed update never holds the sync back.
- systemd reads the whole script as one double-quoted item. The quote starts the item, so the rule
  above holds.
- Four characters in the script need care. The quote rule reads two of them, the specifier rule reads
  one, and the variable rule reads one.

| Character | Read by | Written as |
|---|---|---|
| `\` | the quote rule | `\\` |
| `"` | the quote rule | `\"` |
| `%` | the specifier rule | `%%` |
| `$` | the variable rule | `$$` |

**Choice: option B.** It reuses the `prefix` that the tests already cover, and the script stays the
same on macOS and on Linux. Option A cannot put the `-` in front of a quoted path. Option B costs one
extra layer of quoting. One function, `systemd_quote`, owns that layer.

Source: <https://man7.org/linux/man-pages/man5/systemd.service.5.html> (sections "Type=", "ExecStart=",
and "Command lines"), <https://man7.org/linux/man-pages/man7/systemd.syntax.7.html> (quotes and C-style
escapes), and <https://raw.githubusercontent.com/systemd/systemd/main/man/standard-specifiers.xml>
(`%%`).

Not verified (L-1). That systemd reads the line as the two quoting layers assume. A scratch test read
the `ExecStart=` line with a reader that follows the documented rules, and it ran the result under
`/bin/sh` with a fake program. It used eight root names. One had no special character. The others held
a space; `%` with `$`; `"`; `\`; `'`; a backquote; and `%h`, `$$`, `${HOME}`, and `\s` together. The fake
program received the exact root each time. That shows that the two layers agree with each other. It does
not show what systemd does. To verify: on Linux, write the unit for a root with those characters, start
the service with a fake program in place of `brainmaker`, and compare the arguments that the program
records.

### 4. Log

**Answer.** `StandardOutput=append:<file>` appends the output of a service to a file. It exists from
systemd 240. Source: `NEWS` of systemd 240, <https://raw.githubusercontent.com/systemd/systemd/v240/NEWS>.
The value `truncate:` came in 248 (<https://raw.githubusercontent.com/systemd/systemd/v248/NEWS>), and
the value `file:` came in 236 (<https://raw.githubusercontent.com/systemd/systemd/v236/NEWS>).

**This design does not use it.** The unit already runs `sh -c`. The script starts with
`exec >> "<log>" 2>&1`, which appends both outputs on every systemd version. The log path then goes
through the shell quoting that the tests already cover. The date line is the same as on macOS: the
script runs `date` next, so the log holds one line for each run.

Not verified (L-2, L-3). Both matter only if the maintainer prefers `append:`. First, whether a space or
a `%` in the path of `append:` needs care. The manual page for `StandardOutput=` is cut off in the
copies that the tool read. Second, what systemd older than 240 does with the value. The `NEWS` file says
when the value arrived, and nothing more. To verify: read `man systemd.exec` on a Linux system, and run
a unit that uses `append:` on systemd 239.

Not verified (L-4). That the `PATH` of the user manager holds the folder of `date`. The script runs
`date` by name, as the macOS script does. To verify: run `systemctl --user show-environment` on
Debian, Fedora, and Arch.

### 5. Load and unload

**Answer.** Four commands, all with `--user`. Source: <https://man7.org/linux/man-pages/man1/systemctl.1.html>.

| Step | Command | What the manual says |
|---|---|---|
| Read the files | `systemctl --user daemon-reload` | Reloads all unit files and builds the dependency tree again |
| Start at each login | `systemctl --user enable brainmaker.timer` | Creates the symbolic links that `[Install]` names, then reloads the configuration |
| Start now | `systemctl --user restart brainmaker.timer` | Stops the unit and starts it. Starts a unit that is not running |
| Remove | `systemctl --user disable --now brainmaker.timer` | Removes the links, and `--now` stops the unit |

`daemon-reload` makes the manager read files that changed while it ran. `enable` also reloads, but
`link` can run again on files that changed under a timer that is already enabled. `install` runs
`daemon-reload`, `enable`, and `restart`, in that order. It uses `restart` so that a timer that
already runs starts again with the new files. The manual does not say what `daemon-reload` alone does
to a running timer.

`remove` runs `disable --now brainmaker.timer` and `stop brainmaker.service`, deletes the files, and
runs `daemon-reload`. `stop` ends a run that is still going. `remove` ignores the errors of these
commands, as the macOS code ignores the errors of `bootout`. `install` compares the text of both files
with the files on disk. When both are equal, it runs no command, as on macOS.

`[Install]` holds `WantedBy=timers.target`, and `enable` makes the link from that line. The manual for
special units lists `timers.target` among the units of the user manager. Source: the section
`[Install]` of <https://man7.org/linux/man-pages/man5/systemd.unit.5.html>, and
<https://man7.org/linux/man-pages/man7/systemd.special.7.html>.

Not verified (L-9). That the user manager pulls in `timers.target` at each login. User timers with
`WantedBy=timers.target` are common practice, but the pages above do not say so. To verify: enable
the timer, log out and in, and read `systemctl --user list-timers`.

**No user manager.** Over SSH without a session, after `sudo -u`, and in a container, `systemctl --user`
can fail to reach a manager. `pam_systemd` creates the runtime folder `/run/user/<uid>` and starts the
manager when the first session opens. A login that skips `pam_systemd` starts neither. By decision 7
that is a notice. The files stay in place. The notice names the command to run in a login session:

```text
notice: cannot start the timer: <error>. Run systemctl --user enable --now brainmaker.timer from a login session.
```

Unlike launchd, systemd does not start a timer at the next login from the file alone. The timer needs
the link that `enable` makes. The design does not make that link by hand. It would copy what `enable`
does, and it would help only in the rare case of a `link` with no manager.

Not verified (L-5). The exact error text of `systemctl --user` without a manager, and that `enable`
needs the manager when systemd boots the system. The pages that the tool read do not say. To verify:
run `env -u XDG_RUNTIME_DIR systemctl --user status` in a container, and read the message.

Not verified (L-7). The path of `systemctl`. It is `/usr/bin/systemctl` on a system with the merged
`/usr`. On a Debian system without the merge it can be `/bin/systemctl`. The code names both paths
and takes the first file that exists. To verify: run `dpkg -L systemd` on Debian 11.

### 6. Lingering

**Answer.** A user manager stops after the last logout, unless lingering is on. With lingering, the
system starts a user manager at boot and keeps it after logouts. Source:
<https://man7.org/linux/man-pages/man1/loginctl.1.html>. `loginctl enable-linger` changes a setting of
the machine, and decision 3 says that the agent belongs to the account.

**Recommendation: do not ask for lingering.** A laptop has sessions. A server with no session runs no
Claude session, so no one reads the content, and the hook stays the trigger there. The maintainer
decides (see the open questions).

Not verified (L-6). Whether a user without privileges can run `loginctl enable-linger` for their own
account. The manual page does not say. To verify: run it as a normal user on Debian and on Fedora.

### 7. Systems without systemd

**Answer.** `install` tests whether the folder `/run/systemd/system` exists. That is the test of
`sd_booted()`, which tests for the same folder. Source:
<https://man7.org/linux/man-pages/man3/sd_booted.3.html>. When the folder is missing, `install` writes
no file and prints one notice:

```text
notice: this system does not run systemd, so link writes no hourly agent. The SessionStart hook is the only trigger.
```

`install` runs the test only when the agent would load. A test with `--agent-dir` still writes the files
on any system. `remove` needs no test: it deletes the files that it finds. A container without systemd
as init has no such folder, so it gets the notice. The manual page names no exception for a container or
for a chroot.

### 8. Static builds

**Answer.** The link type does not matter. systemd starts `/bin/sh`, and the shell starts the file at
the absolute path. The binary needs no library from the system, so a musl build runs as any build
does. Source: <https://man7.org/linux/man-pages/man5/systemd.service.5.html>. It says that systemd
starts the program that `ExecStart=` names.

### The environment of the agent

The user manager gives the agent its own environment, and not the environment of the login shell.
`ureq` reads `ALL_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY`, and `NO_PROXY` from the environment by
default (<https://docs.rs/ureq/3.4.2/ureq/struct.Proxy.html> and
<https://docs.rs/ureq/3.4.2/ureq/config/struct.Config.html>). A proxy that a shell profile sets
reaches the hook, and it does not reach the agent. The macOS agent has the same limit (not checked).
On Linux a file in `~/.config/environment.d/` fixes it. Source:
<https://man7.org/linux/man-pages/man5/environment.d.5.html>.

### The Linux files

The files below are the output of `render_systemd` for a user `you`. The line that starts with
`ExecStart=` is one line.

`brainmaker.service`:

```ini
# Written by brainmaker link. brainmaker unlink removes it.
[Unit]
Description=Update brainmaker and the shared content

[Service]
Type=oneshot
ExecStart=/bin/sh -c "exec >>\"/home/you/.brainmaker/agent.log\" 2>&1; date; \"/home/you/.brainmaker/bin/brainmaker\" --dir \"/home/you/.brainmaker\" self-update --quiet; \"/home/you/.brainmaker/bin/brainmaker\" --dir \"/home/you/.brainmaker\" sync --quiet --no-update-check"
TimeoutStartSec=30min
```

`brainmaker.timer`:

```ini
# Written by brainmaker link. brainmaker unlink removes it.
[Unit]
Description=Run brainmaker every hour and after each login

[Timer]
OnCalendar=*-*-* *:00:00
AccuracySec=1s
OnStartupSec=1min

[Install]
WantedBy=timers.target
```

Two settings need a reason.

- The service sets no `RemainAfterExit=`. A one-shot service that stays active would keep the timer
  from starting it again, because a timer does not restart a unit that is already active. Source:
  <https://man7.org/linux/man-pages/man5/systemd.timer.5.html>.
- `TimeoutStartSec=30min` is a backstop. A one-shot service has no start timeout by default, so a hung
  run would block every later run. The program limits itself: a connection times out after 10
  seconds, a text request after 20 seconds, and a download after 300 seconds (`src/remote.rs`). A
  normal run never reaches 30 minutes. Source for the default:
  <https://man7.org/linux/man-pages/man5/systemd.service.5.html>.

Not verified (L-10). That `systemd-analyze verify --user` runs without a user session, as on a CI
runner. The manual says that `verify` loads the unit files, checks the `ExecStart=` programs, and
searches the folder of each file. It does not say that `verify` needs a manager or a runtime folder.
The test `systemd_accepts_the_units` sets `XDG_RUNTIME_DIR` when it is not set. It runs only on
Linux, and it did not run here. To verify: read the result of the first CI run on Linux. Source:
<https://man7.org/linux/man-pages/man1/systemd-analyze.1.html>.

## Windows

### 1. Mechanism

**Answer.** The agent is a scheduled task of the current user. `link` writes an XML file and runs
`%SystemRoot%\System32\schtasks.exe /Create /TN <name> /XML <file> /F`. The option `/F` creates the task
without a warning when a task of that name exists. Source:
<https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/schtasks-create>.

**The alternative is `/SC HOURLY` with `/TR`.** The XML form is better here for four reasons.

1. `/TR` names one program. The manual says that each task runs one program. An XML task can hold
   several actions (at most 32). Source: the same page, and
   <https://learn.microsoft.com/en-us/windows/win32/taskschd/task-actions>.
2. One `/Create` has one schedule type. An XML task holds several triggers (at most 48), so one task
   has the hourly trigger and the logon trigger. Source:
   <https://learn.microsoft.com/en-us/windows/win32/taskschd/task-triggers>.
3. `/SC ONLOGON` is unclear about the user. The `/sc` row of the manual says that the task runs when
   any user logs on. The `/mo` row says that it runs when the user of `/ru` logs on. The XML
   `LogonTrigger` has a `UserId` element. Source: the create page, and
   <https://learn.microsoft.com/en-us/windows/win32/taskschd/logontrigger-userid>.
4. `/TR` is one command line with its own quoting. The manual does not say how to quote a path with a
   space, together with arguments.

**A password prompt can block `link`.** The manual says that `schtasks` always asks for a password unless
one is given, even for a task on the local computer under the current user. `Command::output` closes
standard input, so a prompt cannot block. It fails, and by decision 7 that is a notice. Source:
<https://doc.rust-lang.org/std/process/struct.Command.html> (method `output`).

### 2. Schedule

**Answer.** The task has two triggers and a set of settings.

- **Every hour at minute 0.** A `TimeTrigger` with a `StartBoundary` on a whole hour, and a
  `Repetition` with `<Interval>PT1H</Interval>` and no `Duration`. The manual says that a repetition with
  no duration repeats without end. The interval is at least 1 minute and at most 31 days. The
  `StartBoundary` is a fixed date in the past, so the text of the file does not change between two runs
  of `link`. Source:
  <https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-duration-repetitiontype-element>,
  <https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-interval-repetitiontype-element>,
  and <https://learn.microsoft.com/en-us/windows/win32/taskschd/time-trigger-example--xml->.
- **At log on of this user.** A `LogonTrigger` with `<UserId>` and `<Delay>PT1M</Delay>`. `UserId` is a
  user name or a SID, and `Delay` is the time between the logon and the start. Without a `UserId` the
  trigger fires for any user. Source:
  <https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-logontriggertype-complextype>
  and <https://learn.microsoft.com/en-us/windows/win32/taskschd/logon-trigger-example--xml->.
- **A missed run.** `StartWhenAvailable` set to `true` lets Task Scheduler start the task after the
  scheduled time has passed. It applies only to timed tasks. This matches macOS, where launchd makes up
  a missed run when the Mac wakes. Source:
  <https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-startwhenavailable-settingstype-element>.
- **Battery.** `DisallowStartIfOnBatteries` and `StopIfGoingOnBatteries` are both `true` when the file
  does not name them. A laptop on battery would not start the agent. The task sets both to `false`.
  Source: <https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-settingstype-complextype>.
- **Two runs at once.** `MultipleInstancesPolicy` is `IgnoreNew` by default. A run that is still going at
  the next hour is not started twice. The task names the value. Source:
  <https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-multipleinstancespolicy-settingstype-element>.
- **A hung run.** A task stops 72 hours after it starts, unless `ExecutionTimeLimit` says otherwise. The
  task sets `PT30M`. Source:
  <https://learn.microsoft.com/en-us/windows/win32/taskschd/tasksettings-executiontimelimit>.

Not verified (W-6). That the first hourly run falls on minute 0 when the `StartBoundary` is in the
past, and that `StartWhenAvailable` also makes up a missed repetition, and not only a missed
`StartBoundary`. To verify: register the task, put the machine to sleep across minute 0, and read
"Last Run Time" in Task Scheduler.

Not verified (W-8). The order of the elements inside a trigger. The schema page says that a trigger is a
sequence: `Enabled`, `StartBoundary`, `EndBoundary`, `Repetition`, `ExecutionTimeLimit`, and then the
elements of the trigger type. The task follows that order. The examples on Microsoft pages use other
orders, so `schtasks.exe` can accept any order. To verify: run `/Create /XML` on the task text.
Source: <https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-triggerbasetype-complextype>.

### 3. Command

**Answer.** A task can hold more than one `<Exec>` action, and Task Scheduler runs the actions in
order. Source: <https://learn.microsoft.com/en-us/windows/win32/taskschd/task-actions>.

The manual does not say what the second action does when the first exits with a code other than 0.
Neither the page for actions nor the page for the `Actions` element mentions it. Decision 1 needs the
answer. Source for the second page:
<https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-actions-tasktype-element>.

Not verified (W-1). Whether the second action runs after the first fails. The design does not depend on
it. To verify: register a task with the actions `cmd.exe /c exit 1` and `cmd.exe /c echo ok > file`, run
it, and look for the file.

**The fallback.** One action starts `cmd.exe`, and `cmd.exe` runs both commands. Two other fallbacks
exist:

- A new command of `brainmaker` that runs the update and then the sync. It removes `cmd.exe` and its
  quoting, and it can write the log. It adds a command to the interface.
- Two tasks, one for each command. The order is lost, and there are two names to manage.

**Choice: `cmd.exe`.** It keeps decision 1 and it changes only `src/schedule.rs`. The script is:

```text
(echo %date% %time% & <prefix> self-update --quiet & <prefix> sync --quiet --no-update-check) >> "<log>" 2>&1
```

`&` puts two commands on one line. Only `&&` and `||` make the second command depend on the first. So
`sync` runs after a failed `self-update`, as decision 1 says. Source:
<https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/cmd>.

`cmd.exe` starts with `/d /v:off /s /c`. Each switch has a reason:

| Switch | Meaning in the manual |
|---|---|
| `/d` | Disables AutoRun commands, so a registry entry of the user cannot run first |
| `/v:off` | Disables delayed expansion, so `!` in a path is text |
| `/s` | Strips the first and the last quote of the string and leaves the rest unchanged |
| `/c` | Runs the string and then exits |

**Rules for the paths in the string.** The build must apply them. `link` prints a notice and writes no
agent when a path breaks one of them.

1. Put each path in double quotes. The manual says that `&`, `|`, and parentheses need `^` or quotes
   when they are arguments.
2. Refuse a path with `%`. `cmd.exe` replaces `%name%` with the value of a variable, and the page gives
   no way to write a literal percent sign on a command line.
3. Refuse a path with a double quote or with a control character.
4. Double each run of backslashes at the end of a path, before the closing quote. The program reads its
   command line by the rules of the C runtime. There `\"` is a literal quote, and an even number of
   backslashes before a quote gives half as many. The Rust runtime follows these rules. Source:
   <https://learn.microsoft.com/en-us/cpp/cpp/main-function-command-line-args> (section "Parsing C++
   command-line arguments") and `library/std/src/sys/args/windows.rs` in
   <https://github.com/rust-lang/rust>.

The `prefix` of `link::command_prefix` is shell quoting, and it doubles every backslash. On Windows the
build ignores that `prefix` and makes its own from the root. Section "Code" says how.

Not verified (W-2). Three points about the text that Task Scheduler passes on. `Arguments` reaches
`cmd.exe` as written. `%SystemRoot%` in `Command` is expanded. `%date%` in `Arguments` is left alone.
The pages for `Command` and `Arguments` say nothing about quotes or variables. The task writes
`%SystemRoot%\System32\cmd.exe` in `Command`, in line with the rule to call a system program by its
full path. To verify: register the task from the text below, run it with `schtasks /Run`, and read
`agent.log`. If `%SystemRoot%` fails, the build writes the resolved path.

### 4. No window

**Answer.** No documented setting hides the console window of a task that a standard account can
register.

- `Hidden` hides the task in the Task Scheduler window, and not the console window. Source:
  <https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-hidden-settingstype-element>.
- A task with `InteractiveToken` runs in an existing interactive session. A task that runs "whether user
  is logged on or not" does not run interactively. The pages do not name the console window. Source:
  <https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-logontype-principaltype-element>
  and <https://learn.microsoft.com/en-us/previous-versions/windows/it-pro/windows-server-2008-R2-and-2008/cc722152(v=ws.11)>.
- A task with `S4U` or `Password` starts only when the user has the right "Log on as a batch job". The
  pages name only Administrators and Backup Operators as holders by default. With `S4U` the task has no
  access to the network or to encrypted files, and a task that needs network resources cannot use it.
  The archived page lists one exception. Source:
  <https://learn.microsoft.com/en-us/windows/win32/taskschd/security-contexts-for-running-tasks> and the
  archived page above.
- `conhost.exe --headless` is not in the Microsoft documentation. Two issues in the repository of the
  Windows Terminal project describe it. Issue 17178 says that it returns exit code 0 whatever the child
  returns, and the project closed it as not planned. Issue 13914 says that it stopped working in one
  build. Source: <https://github.com/microsoft/terminal/issues/17178> and
  <https://github.com/microsoft/terminal/issues/13914>.

**Choice.** The task uses `InteractiveToken`. The expected cost is a console window for the seconds of
each run, at each hour and after each logon (W-5). The other documented choices fail for a standard account:
`S4U` registers, and then the task does not start without the batch right. The maintainer can still
choose to skip the agent on Windows (see the open questions).

Not verified (W-5). That a console window shows under `InteractiveToken`, and that `S4U` allows an
outbound HTTPS connection. To verify: run the registered task on a Windows 11 desktop and watch, then
register it with `S4U` from an administrator account and read `agent.log`.

### 5. Rights

**Answer.** Yes. A user without administrator rights can register a task for their own account that runs
only while they are logged on. The page on security contexts says that such a user needs no password
when the task runs under their own account with the `S4U` or the interactive logon type. From a
low-privilege process, a task with `RunLevel` of `HighestAvailable` fails, and one with `LeastPrivilege`
works. The values are `<LogonType>InteractiveToken</LogonType>` and `<RunLevel>LeastPrivilege</RunLevel>`.
Source: <https://learn.microsoft.com/en-us/windows/win32/taskschd/security-contexts-for-running-tasks>.

Members of the Users group can read, update, delete, and run only the tasks that they created.
Administrators can do so for every task. A task name is unique for the machine, not for the user. A
second user on the same machine must not overwrite the task of the first. The task name therefore holds
the user name: `brainmaker-<user>`. Source: the same page, and the create page.

Not verified (W-4). The sources disagree. The create page says, in the section on the System account,
that only administrators can schedule tasks, whatever the value of `/ru`. The page on security contexts
says that a user without administrator rights can register a task for their own account. Also, no page
says that such a user can register a `LogonTrigger` with their own `UserId`. To verify: run
`schtasks /Create /XML` on the task text from a standard account.

### 6. Log

**Answer.** `<Exec>` has three child elements: `Command`, `Arguments`, and `WorkingDirectory`. It cannot
redirect output. Source: <https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-exectype-complextype>.
Two options send the output to `<root>\agent.log`.

- **`cmd.exe /c "... >> file 2>&1"`.** It needs no change in the program. The quoting risk is real. `/c`
  has a rule for quotes that can remove the wrong quote when `/s` is missing. `cmd.exe` replaces
  `%name%`, and the manual does not say that quotes stop it. A path that ends in a backslash breaks the
  program's own reading of its arguments. The rules in section 3 remove these risks for every path
  except one with `%`, which the build refuses.
- **A new option of `brainmaker` that names a log file.** It removes the shell from the log, but not from
  the join of the two commands. It adds an option to `cli.rs`, `main.rs`, and API.md.

**Recommendation: `cmd.exe`.** The join of the two commands already needs `cmd.exe`, so the second
option removes only one of two reasons for the shell. Section 3 names the larger fix, a new command.

Not verified (W-3). The encoding of the XML file. The manual pages do not name it. A forum thread that
Microsoft hosts has replies that disagree on UTF-8 with a UTF-16 declaration. Several replies agree that
a UTF-16 little-endian file works, and one says that UTF-16 big-endian and UTF-8 do not work. The error
in the thread is "ERROR: The task XML is malformed". The task text declares `encoding="UTF-16"`. The
build writes the bytes `FF FE` and then the text as UTF-16 little-endian. A reply in the thread says that
Task Scheduler exports a task in that form. `xmllint` accepted a file of that form, and it refused the
same text as UTF-8 with the same declaration. To verify: run `schtasks /Create /XML` on that file, and on
the same text as UTF-8. Source:
<https://learn.microsoft.com/en-us/archive/msdn-technet-forums/cdc10106-11b4-4ed4-b637-b33f0c1ce01c>.

### 7. Load and unload

**Answer.** Four commands. Each runs by full path, with its arguments as a list, and with standard
input closed.

| Step | Command | What the manual says |
|---|---|---|
| Create or replace | `schtasks.exe /Create /TN <name> /XML <file> /F` | `/F` creates the task and suppresses warnings when it exists |
| Stop a run | `schtasks.exe /End /TN <name>` | Stops only the instances of a program that the task started |
| Delete | `schtasks.exe /Delete /TN <name> /F` | Deletes the task. It does not delete the program or interrupt a running program |
| Read | `schtasks.exe /Query /TN <name> /XML` | Prints the definition of the task |

Source: the pages `schtasks-create`, `schtasks-end`, `schtasks-delete`, and `schtasks-query` under
<https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/>.

`schtasks.exe` is in `C:\Windows\System32`. Source: the page
<https://learn.microsoft.com/en-us/windows/win32/taskschd/time-trigger-example--xml-> says so. The code
builds the path from the `SystemRoot` environment variable and never uses the bare name. Plan 013 gives
the code that reads the machine identifier the same rule, and it calls `%SystemRoot%\System32\reg.exe`.

`remove` runs `/End`, then `/Delete`, and it ignores their errors. Then it deletes the XML file, and the
folder when the folder is empty. `/End` stops a run that still holds `agent.log` open, so `uninstall`
can delete the log.

Not verified (W-7). Three points. Whether `schtasks /Create /XML` asks for a password when the task has
the logon type `InteractiveToken`. Whether `/F` replaces the definition of a task that exists. The exit
status of `schtasks /Query` for a task that does not exist: the `schtasks-query` page does not give it.
To verify: run the three cases from a standard account, and read `%ERRORLEVEL%`.

### 8. The running program

**Answer.** An hourly run changes nothing about the way `self-update` replaces a running program. It
renames the running file to `.brainmaker-old.exe`, and it puts the new file in place. The code assumes
that the rename works while the file runs. Windows refuses to delete a program while it runs, so the
delete of the old file fails, and the code ignores that error (`let _ = fs::remove_file(&backup)` in
`src/selfupdate.rs`). The next update deletes the file, and `uninstall` deletes every `.brainmaker*`
file in `bin/`.

Only an update leaves the file, and an update is rare. A run with no newer version calls no swap, so
the hourly run leaves nothing. When the agent runs the program copy under the root, the running file
is the installed copy. The swap replaces it, and no second copy is needed.

Not verified (W-11). That Windows lets a process rename its own running executable. The existing code
depends on it. To verify: run `self-update` on Windows while a second copy of the same file runs.

### 9. The hook on Windows

**Answer.** `link` can fail on a standard Windows account today, so the agent is the smaller problem.
`link` makes one directory symbolic link for each skill (`std::os::windows::fs::symlink_dir`). The Rust
manual says that Windows treats the creation of a symbolic link as a privileged action. It says that a
user can try Developer Mode, the `SeCreateSymbolicLinkPrivilege` privilege, or an administrator process.
The Windows manual says that the flag for unprivileged creation works only after Developer Mode is on.
The standard library passes that flag. So an account with no rights and no Developer Mode cannot make
the links. When the content ships a skill, `link_skills` runs first and its error ends `link`, so no
hook, no briefing, and no agent follow. Source:
<https://doc.rust-lang.org/std/os/windows/fs/fn.symlink_dir.html> and
<https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-createsymboliclinkw>.

The build of the Windows agent must wait for an answer to two questions. Does the fleet use Developer
Mode, or does `link` need a fallback such as a directory junction? Which shell runs the hook command on
Windows? The hook uses POSIX shell quoting.

Not verified (W-10). Both questions. To verify: run `link` on a standard Windows account with Developer
Mode off, then on. Read the hook documentation of Claude Code for the shell.

### The Windows file

The text below is the output of `render_task(prefix, log, user)` for the user `PC\you`. The build writes
it as UTF-16 with a byte order mark. The line inside `<Arguments>` is one line.

```xml
<?xml version="1.0" encoding="UTF-16"?>
<!-- Written by brainmaker link. brainmaker unlink removes it. -->
<Task xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Runs brainmaker self-update, then sync, every hour and after each logon.</Description>
  </RegistrationInfo>
  <Triggers>
    <TimeTrigger>
      <Enabled>true</Enabled>
      <StartBoundary>2026-01-01T00:00:00</StartBoundary>
      <Repetition>
        <Interval>PT1H</Interval>
      </Repetition>
    </TimeTrigger>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>PC\you</UserId>
      <Delay>PT1M</Delay>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal>
      <UserId>PC\you</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <ExecutionTimeLimit>PT30M</ExecutionTimeLimit>
    <Enabled>true</Enabled>
  </Settings>
  <Actions>
    <Exec>
      <Command>%SystemRoot%\System32\cmd.exe</Command>
      <Arguments>/d /v:off /s /c "(echo %date% %time% &amp; "C:\Users\you\.brainmaker\bin\brainmaker.exe" --dir "C:\Users\you\.brainmaker" self-update --quiet &amp; "C:\Users\you\.brainmaker\bin\brainmaker.exe" --dir "C:\Users\you\.brainmaker" sync --quiet --no-update-check) &gt;&gt; "C:\Users\you\.brainmaker\agent.log" 2&gt;&amp;1"</Arguments>
    </Exec>
  </Actions>
</Task>
```

XML text cannot hold `&`, `<`, or `>` as they are. `escape` writes `&amp;`, `&lt;`, and `&gt;`, as it does for
the macOS property list. The decoded `Arguments` text is what `cmd.exe` reads.

The task does not set `Hidden`. The user must see the task in Task Scheduler. The user name in both
`UserId` elements is `USERDOMAIN\USERNAME` from the environment when `link` runs. The `Principal` names
the user too, because the create page says that `/XML` takes the account from the file when it holds one.

Not verified (W-9). That `USERDOMAIN\USERNAME` names the account on a machine that joins a domain or uses
a Microsoft account. To verify: run `link` on such a machine, and read the result of `/Create`.

## Code

### 1. One type or three

**Decision: one struct, with a private enum for the backend.** `Agents` stays one public struct. It gets a
field `backend` of a private type `Backend` with three variants: `Launchd`, `Systemd`, and `Task`.
`resolve` picks the variant from the system, and every other function reads the field.

| System | `resolve(None, false)` | `resolve(Some(dir), _)` |
|---|---|---|
| macOS | `~/Library/LaunchAgents`, loads | `dir`, never loads |
| Linux | `$XDG_CONFIG_HOME/systemd/user`, or `~/.config/systemd/user` when the variable is not set, loads | `dir`, never loads |
| Windows | `%LOCALAPPDATA%\brainmaker`, loads | `dir`, never loads |
| any other system | none | `dir`, a property list, never loads |

The build reads these variables and `std::env::home_dir()`, because plan 016 removes the `dirs` crate.

`resolve(None, true)` still returns none on every system, which is decision 6.

**Why not a public enum with one variant for each system.** The callers hold an `Option<&Agents>` and
never look inside it. A public enum would put the system into `link.rs` and `uninstall.rs`. **Why not a
trait.** A trait needs a box or a generic for three variants that a closed enum covers.

**One more reason for the value.** The backend is a value, and not a `cfg` gate, so a test can build any
of the three on any host. `Agents::unloaded_for(Backend::Launchd, dir)` gives a macOS test its backend on
a Linux host. Only `resolve` uses `cfg!`.

The functions that call into `schedule`, at commit `1df0862`:

| Caller | Call | Change |
|---|---|---|
| `main.rs:139`, `agents()` | `Agents::resolve` | none |
| `link.rs:150`, `link()` | `schedule::install(agents, &prefix, root, log)` | none. Windows ignores `prefix` and builds its own from `root` |
| `link.rs:177`, `remove_bridge()` | `schedule::remove(agents)` | none |
| `link.rs:777`, `describe()` | `schedule::LABEL` in "the hourly LaunchAgent" | text: name the kind and the name that the backend gives |
| `uninstall.rs:78`, `question()` | `agents.plist()` in "the hourly LaunchAgent" | text: list `agents.files()` and name the kind |
| `uninstall.rs:193`, `remove_root()` | `schedule::log_path(root)` | none |

The help text in `src/cli.rs`, README.md, API.md, ARCHITECTURE.md, SECURITY.md, and FLOW.md say
"LaunchAgent" and "macOS only". They change with the build. The tests in `uninstall.rs` that name the
LaunchAgent in a string must name the backend.

### 2. `--agent-dir`

**Decision.** `--agent-dir` names the folder that receives the files of the agent, and nothing loads
them. A test can render and write the agent without loading it, on any system.

| System | Files in the folder | What never happens |
|---|---|---|
| macOS | `it.damac.brainmaker.plist` | `launchctl` |
| Linux | `brainmaker.service`, `brainmaker.timer` | `systemctl`, the test for systemd, the link that `enable` makes |
| Windows | `brainmaker.xml`, the task definition | `schtasks.exe` |

On Windows the default folder is `%LOCALAPPDATA%\brainmaker`, and not the root. `resolve` gets no root, and
this folder does not roam between machines. The file is the record of the last definition that `link`
registered, as the property list is on macOS. `install` compares the new text with the file. It registers
the task again when `/Query` fails, for example after a person deleted the task in Task Scheduler.
Decision 6 does not change: a run with `--claude-dir` and no `--agent-dir` writes nothing.

### 3. Names

| System | Name | Reason |
|---|---|---|
| macOS | `it.damac.brainmaker` | Fixed. Apple asks for a reverse domain label |
| Linux | `brainmaker.service`, `brainmaker.timer` | The convention of systemd is the plain name. The folder is for one user, so the name needs no owner. A unit name can hold dots, so the label would also be valid |
| Windows | `brainmaker-<user>`, in the root folder of Task Scheduler | The name must be unique for the machine, and the agent belongs to one account |

The rule for a unit name is in <https://man7.org/linux/man-pages/man5/systemd.unit.5.html>.

### 4. What `uninstall` removes

Each system goes in the same order. The trigger stops first, then the files go. `link::remove_bridge` runs
`schedule::remove` before it removes the skill links, the hook, and the briefing block. `remove_root`
then deletes the program copy and `agent.log`. That order is decision 5.

| System | Order inside `schedule::remove` |
|---|---|
| macOS | `launchctl bootout`, delete the property list |
| Linux | `systemctl --user disable --now brainmaker.timer`, `stop brainmaker.service`, delete both unit files, delete the link `timers.target.wants/brainmaker.timer` when `disable` failed and the link is still there, `daemon-reload` |
| Windows | `schtasks /End`, `schtasks /Delete /F`, delete `brainmaker.xml`, delete the folder when it is empty |

### 5. What `status` shows

The change that adds agent rows to `status` needs one definition of "loaded" for each system.

| System | "Loaded" means | Command |
|---|---|---|
| macOS | launchd knows the job | `launchctl print gui/<uid>/it.damac.brainmaker`, exit status 0 |
| Linux | the timer is enabled and active | `systemctl --user is-enabled brainmaker.timer` and `is-active brainmaker.timer`. Each prints a word and exits with 0 for `enabled` and for `active` |
| Windows | the task exists and is enabled | `schtasks.exe /Query /TN <name> /XML`: exit status 0 and `<Enabled>true</Enabled>` in `Settings` |

Source for Linux: <https://man7.org/linux/man-pages/man1/systemctl.1.html>. Source for Windows: the
`schtasks-query` page.

`status` must not parse `schtasks /Query /V /FO LIST`. The sample output in the manual has English labels,
and the labels can follow the display language of Windows. The XML is the same in every language.

### 6. The prototype in `src/schedule.rs`

Two functions render text, and both carry `cfg(test)` until the build. Each test pins one decision.

| Test | What it pins |
|---|---|
| `the_systemd_units_run_the_update_and_then_the_sync` | One `ExecStart=`, `;` and not `&&`, no `RemainAfterExit=`, the timeout |
| `the_timer_fires_at_minute_zero` | `OnCalendar=*-*-* *:00:00`, `AccuracySec=1s`, `OnStartupSec=1min`, no `Persistent=` |
| `the_systemd_units_carry_a_path_that_holds_a_space` | The whole `ExecStart=` line for a path with a space, and the four escapes |
| `systemd_accepts_the_units` | Linux only. `systemd-analyze verify --user` accepts both files |
| `the_task_runs_for_the_user_and_stays_visible` | The principal, both triggers, the settings, one action, `&`, no `Hidden` |
| `the_task_escapes_a_path_for_xml` | `&amp;` and no bare `&`, and no angle bracket inside `Arguments` |

`render_task` takes a third argument, `user`, because the logon trigger names the account (Windows 2 and
5). The fifth test says `stays_visible` and not `hides_its_window`, because no documented setting hides
the window (Windows 4). `render_systemd` and `render_task` take the `prefix` of the platform: shell
quoting on Linux, and quoting for `cmd.exe` on Windows.

## Build steps

The steps run in order. Linux comes first, because CI runs on Linux. Each step names the file that it
changes and the test that proves it.

**Before any step.** The run lock is in the tree (`src/lock.rs`). No agent ships on a new platform without it.

**Linux**

1. **Backend as a value.** In `src/schedule.rs`, add `Backend` and the field `backend` to `Agents`. Add
   `Agents::unloaded_for`. `resolve` picks the backend. The tests that depend on the system name their
   backend. Proof: the tests of `schedule.rs` pass on macOS, Linux, and Windows.
2. **The unit renderer.** In `src/link.rs`, make `shell_quote` `pub(crate)`. In `src/schedule.rs`, delete
   `shell_word`, call `shell_quote`, and remove `#[cfg(test)]` from `render_systemd` and `systemd_quote`.
   `render_systemd` returns a `Result`, because `shell_quote` refuses a control character. Proof: the four
   prototype tests, and a new test `a_systemd_line_reads_back` on Unix. That test reads the `ExecStart=`
   line with a reader of the documented rules, runs it under `/bin/sh` with a fake program, and compares
   the arguments. It uses the eight root names of L-1.
3. **Write and remove.** In `src/schedule.rs`, `install` and `remove` for the systemd backend: the test for
   `/run/systemd/system`, both files, the `systemctl` calls (`/usr/bin/systemctl`, then `/bin/systemctl`),
   the notices, and the removal of a dangling link. Proof: new tests
   `writes_the_units_once_and_removes_them_again` and `a_dangling_wants_link_is_removed`, on every host,
   and `a_system_without_systemd_gets_one_notice`. The inner function that `install` calls takes the result
   of the systemd test as an argument, so a test can pass either value.
4. **Wording.** In `src/link.rs` (`describe`), `src/uninstall.rs` (`question`), and `src/cli.rs` (help text),
   name the kind and the files of the agent. Proof: the tests that name the LaunchAgent in a string now
   name the backend, and `cli::tests::the_help_text_names_no_endpoint` still passes.
5. **CI.** `.github/workflows/test.yml` needs no change: `systemd_accepts_the_units` runs in the Linux
   job. Proof: the first Linux run passes, which answers L-10.
6. **Documents.** README.md, API.md, ARCHITECTURE.md, FLOW.md, and `docs/.docsgen.json`: remove "Windows
   and Linux get no agent", and describe the two unit files and the option `--agent-dir` for Linux.
   SECURITY.md: see the last product decision below.
7. **Check on a Linux machine** before the merge, for L-1 to L-9, with the checks in the last table.

**Windows**

8. **Before any code.** The maintainer answers the first three product decisions. A person runs the checks
   for W-1 to W-11 on a real Windows machine, with a scratch task and `schtasks.exe`.
9. **The task renderer.** In `src/schedule.rs`, add `windows_prefix(root)` with the four rules of section
   3, refuse the agent with a notice when a rule fails, and remove `#[cfg(test)]` from `render_task`.
   Write the file as UTF-16 with a byte order mark. Proof: the prototype tests, tests for each refusal
   rule, and a Windows test `cmd_reads_the_arguments_back`. That test runs the `Arguments` text through
   `cmd.exe /d /v:off /s /c` with a fake program, for roots with a space, `&`, `(`, `)`, and `'`.
10. **Register and remove.** In `src/schedule.rs`, `install` and `remove` for the task backend: the calls
    of section 7, the folder `%LOCALAPPDATA%\brainmaker`, and the notices. Proof: text tests on every
    host, and a Windows test that registers a task and deletes it. That test runs only when the variable
    `BRAINMAKER_TEST_SCHTASKS` is set, and the CI job on Windows sets it. A developer machine then
    keeps its own tasks.
11. **Documents.** The same files as step 6, for Windows.

## Open questions for the maintainer

### Product decisions

1. **Does anyone on the team run Windows today?** If no, the Windows half waits. Steps 8 to 11 do not run,
   and Windows keeps the hook as its only trigger.
2. **Must `link` work for a standard Windows account with no Developer Mode?** Today it can fail (W-10).
   A directory junction is one fallback. It is a change to `link`, and not to the agent.
3. **Is a console window each hour acceptable on Windows?** The other choices fail for a standard account
   (section Windows 4). One option is to ship the agent for administrators and for Developer Mode only.
   Another is to skip Windows.
4. **Is lingering wanted?** The recommendation is no. Then a Linux machine with no session gets no agent.
5. **Is a machine without systemd in the fleet?** If none, the notice of section Linux 7 never shows. It
   stays, because a container or WSL can run `link`.
6. **Does any machine need a proxy?** If so, the agent needs the variables in `~/.config/environment.d/`
   on Linux, and in the user environment on Windows.
7. **Should one new command replace the shell layers on all three systems?** A command such as
   `brainmaker agent` would run the update, then the sync, and write the log. It removes `sh -c` and
   `cmd.exe`, and their quoting rules. It adds a command to the interface, and it changes the shipped macOS
   agent.
8. **Should the start times spread?** Every agent starts at minute 0. A fleet of many machines makes one
   spike on the server each hour. `RandomizedDelaySec=` on Linux and a random delay on Windows can spread
   it, and decision 2 fixes minute 0.
9. **Is the signing key allowed to decide what every machine runs?** This is a security decision.
   SECURITY.md says that a signed build "reaches every linked Mac within an hour". With this design it
   reaches every linked Linux and Windows machine within an hour, with no person in between. The five
   controls of `self-update` are then the only gate on all three systems, and the signing key decides. The
   build must change that sentence, and the maintainer must read the new one.

### Answers

The maintainer answered four product decisions on 2026-09-29. The others stay open.

| Decision | Answer |
|---|---|
| 1 | Yes. The team runs Windows today, so the Windows half does not wait. |
| 2 | Open. |
| 3 | Open. |
| 4 | Open. The recommendation stays no. |
| 5 | No. The fleet has no Linux machine without systemd. The notice of section Linux 7 stays, because a container or WSL can run `link`. |
| 6 | No. No machine needs a proxy. |
| 7 | Yes. One new command replaces the shell layers on all three systems. Decision D-5 of status-and-agent-health.md took this answer. |
| 8 | Open. |
| 9 | Open. This is a security decision. |

### Not verified

Each row is an answer that this document could not confirm from a public source.

| ID | What is not verified | How to verify |
|---|---|---|
| L-1 | That systemd reads the `ExecStart=` line as the two quoting layers assume | Write the unit for a root with a space, `%`, `$`, `"`, `\`, and `'`. Start it with a fake program, and compare the arguments that the program records |
| L-2 | Whether a space or `%` in `StandardOutput=append:` needs care (only if `append:` is chosen) | Read `man systemd.exec` on Linux. Run a unit with such a path |
| L-3 | What systemd older than 240 does with the value `append:` (only if `append:` is chosen) | Run a unit that uses `append:` on systemd 239 |
| L-4 | That the `PATH` of the user manager holds the folder of `date` | `systemctl --user show-environment` on Debian, Fedora, and Arch |
| L-5 | The error text of `systemctl --user` without a manager, and that `enable` needs the manager | `env -u XDG_RUNTIME_DIR systemctl --user status` in a container |
| L-6 | Whether a normal user can run `loginctl enable-linger` for their own account | Run it as a normal user on Debian and on Fedora |
| L-7 | The path of `systemctl` on a Debian system without the merged `/usr` | `dpkg -L systemd` on Debian 11 |
| L-8 | Whether `OnStartupSec=1min` fires at once when the timer starts after that time | `enable --now` in an old session, then `systemctl --user list-timers` |
| L-9 | That the user manager pulls in `timers.target` at each login | Enable the timer, log out and in, then `systemctl --user list-timers` |
| L-10 | That `systemd-analyze verify --user` runs on a CI runner with no user session | The first CI run on Linux |
| W-1 | Whether the second `<Exec>` runs after the first exits with a code other than 0 | A task with `cmd.exe /c exit 1` and then `cmd.exe /c echo ok > file` |
| W-2 | That `Arguments` reaches `cmd.exe` as written, that `%SystemRoot%` in `Command` is expanded, and that `%date%` is not | Register the task, run it with `schtasks /Run`, read `agent.log` |
| W-3 | That `schtasks /Create /XML` needs UTF-16 with a byte order mark, and accepts it | Register the file as UTF-16 with a byte order mark, and as UTF-8 |
| W-4 | That a standard account can register the task, with a `LogonTrigger` that names its own `UserId` | Run `schtasks /Create /XML` from a standard account |
| W-5 | That a console window shows under `InteractiveToken`, and that `S4U` allows an outbound HTTPS connection | Run the task on a Windows 11 desktop. Register it with `S4U` and read the log |
| W-6 | That the first hourly run falls on minute 0, and that `StartWhenAvailable` makes up a missed repetition | Register the task, sleep the machine across minute 0, read "Last Run Time" |
| W-7 | Whether `schtasks /Create /XML` asks for a password, whether `/F` replaces a task that exists, and the exit status of `/Query` for a task that does not exist | Run the three cases from a standard account, and read `%ERRORLEVEL%` |
| W-8 | The order of the elements inside a trigger | `/Create /XML` on the task text |
| W-9 | That `USERDOMAIN\USERNAME` names the account on a domain machine or with a Microsoft account | Run `link` on such a machine |
| W-10 | Whether `link` works on a standard account, and which shell runs the hook on Windows | Run `link` with Developer Mode off and on. Read the Claude Code hook documentation |
| W-11 | That Windows lets a process rename its own running executable | Run `self-update` while a second copy of the file runs |

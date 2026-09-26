"""The heavy lane: a machine-wide first-come queue for checks, acceptance commands and worktree setup.

A full check can take gigabytes and every core; several at once make each slower until acceptance times out
and a sound change reads as rejected. So heavy commands queue here, DELEGATE_MAX_HEAVY at a time (default 1),
whoever runs them: a supervisor accepting a run, a worktree being set up, an agent checking its own work, or
the caller running `delegate.py lane make check`. Time spent queued never counts against a timeout.

Nothing polls. A ticket is a file named by its arrival time whose owner holds an exclusive flock on it for as
long as it waits or runs; a waiter blocks on a shared flock of the ticket it is waiting for, so the kernel wakes
it the moment that owner finishes or dies. A ticket nobody holds is dead and is removed by whoever sees it.
"""

import fcntl
import json
import os
import time
from contextlib import contextmanager
from pathlib import Path

from .common import DEFAULT_MAX_HEAVY, LANE_HELD, die, now_iso, read_json, setting, state_dir


class Cancelled(Exception):
    """Raised from a signal handler to abandon a wait (a run was stopped while queued)."""



WAITING = {"now": False}  # lets a signal handler know whether raising Cancelled would abandon a wait



def max_heavy():
    value = setting("MAX_HEAVY", str(DEFAULT_MAX_HEAVY))
    if not value.isdigit():
        die(f"DELEGATE_MAX_HEAVY must be a non-negative integer (0 = unlimited), got {value!r}")
    return int(value)



def lane_dir():
    return state_dir() / "lane"



def held(path):
    """True while some process holds the flock of path, i.e. its owner is alive."""
    try:
        with open(path) as handle:
            fcntl.flock(handle, fcntl.LOCK_SH | fcntl.LOCK_NB)
            return False
    except BlockingIOError:
        return True
    except OSError:
        return False



def locked_file(path, content):
    """Create path already flocked by this process: written and locked under a temporary name, then renamed,
    so nobody ever sees it unlocked and takes it for dead. The returned handle keeps the lock."""
    temp = path.with_name(f".{path.name}.tmp")
    handle = open(temp, "w")
    fcntl.flock(handle, fcntl.LOCK_EX)
    handle.write(content)
    handle.flush()
    os.replace(temp, path)
    return handle



def tickets(directory):
    """Live tickets in arrival order; dead ones are removed on the way."""
    out = []
    for path in sorted(directory.glob("*.ticket")):
        if held(path):
            out.append((path, read_json(path, {}) or {}))
        else:
            path.unlink(missing_ok=True)
    return out



def holders():
    """The heavy commands running or queued now: [(label, since, running)]."""
    directory = lane_dir()
    if not directory.is_dir():
        return []
    limit = max_heavy()
    return [(info.get("label"), info.get("since"), not limit or index < limit)
            for index, (_, info) in enumerate(tickets(directory))]



def block_on(path):
    """Sleep in the kernel until the owner of path lets go of it (finishes or dies)."""
    try:
        with open(path) as handle:
            fcntl.flock(handle, fcntl.LOCK_SH)
    except FileNotFoundError:
        pass



@contextmanager
def heavy_slot(label, on_wait=None, account=None, cancelled=None):
    """Hold one heavy slot; yields the seconds spent queued.

    on_wait(ahead, labels) is called before each wait. account is the run directory of the agent that is
    waiting, whose timeout is extended by the wait. cancelled() is checked once a stop can interrupt the wait
    (WAITING is set), so a stop that came earlier is not missed. Nested use (LANE_HELD) and a limit of 0 go
    straight in.
    """
    limit = max_heavy()
    if limit == 0 or os.environ.get(LANE_HELD):
        yield 0.0
        return
    directory = lane_dir()
    directory.mkdir(parents=True, exist_ok=True)
    me = {"pid": os.getpid(), "label": label, "since": now_iso(), "epoch": time.time()}
    with open(directory / ".lock", "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)  # arrival order is creation order
        ticket = directory / f"{time.time_ns():020d}-{os.getpid()}.ticket"
        handle = locked_file(ticket, json.dumps(me))
    marker = Path(account) / f"lane-waiting-{os.getpid()}" if account else None
    marking = locked_file(marker, json.dumps(me)) if marker else None
    try:
        while True:
            live = tickets(directory)
            names = [path.name for path, _ in live]
            position = names.index(ticket.name) if ticket.name in names else 0
            if position < limit:
                break
            if on_wait:
                on_wait(position - limit + 1, [info.get("label") for _, info in live[:position]])
            WAITING["now"] = True
            try:
                if cancelled and cancelled():
                    raise Cancelled()
                block_on(live[position - limit][0])  # the one whose end frees a slot for us
            finally:
                WAITING["now"] = False
        queued = time.time() - me["epoch"]
        if marker:
            marker.unlink(missing_ok=True)
            marking.close()
            marker = None
            with open(Path(account) / "lane-wait", "a") as log:
                log.write(f"{queued:.1f}\n")
        yield queued
    finally:
        ticket.unlink(missing_ok=True)
        handle.close()
        if marker:
            marker.unlink(missing_ok=True)
            marking.close()



def queued_seconds(run):
    """Seconds an agent has spent waiting in the lane so far, including a wait still going on."""
    run = Path(run)
    total = 0.0
    try:
        total = sum(float(line) for line in (run / "lane-wait").read_text().split())
    except (OSError, ValueError):
        pass
    for marker in run.glob("lane-waiting-*"):
        info = read_json(marker)
        if info and held(marker):
            total += max(0.0, time.time() - info.get("epoch", time.time()))
    return total

#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Presses Space and Escape in the real window of the player and checks what they do.

    python scripts/check-window-keys.py <path to takt-player>

What no unit test sees: that a key pressed in the window reaches `app::command_for` (winit, egui, the window). The
player starts with the test picture and simulated sound (TAKT_AUDIO=sim); Space has to pause it and start it again,
Escape has to end it with exit code 0. A desktop sheet of testing.nvim cannot press keys yet, so this stands beside
`TESTS/sheets/test-picture-window.sheet.lua` until it can.

Windows: the keys are window messages to the window of the process (it needs no focus); whether it plays is the name
of the button (`Pause` or `Play`), read through `sheet-driver` of appkit (on the PATH, or TESTING_DESKTOP_DRIVER).
Linux: X11 only (a display in DISPLAY, e.g. `Xvfb :99`; needs libX11, libxcb-xtest and, for the player,
libxkbcommon-x11); the keys are XTest events, and whether it plays is told from two pictures of the window 0.6 s
apart: the test picture moves.

Exit 0 when all of it holds, 1 when a check fails, 2 for a wrong call or a missing tool.
"""

import ctypes
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time

WINDOW = "Takt"
START_SECONDS = 30
QUIT_SECONDS = 10


def report(what, good, more=""):
    print(f"{'ok  ' if good else 'FAIL'} {what}{' (' + more + ')' if more else ''}")
    return good


def quits(app):
    try:
        code = app.wait(timeout=QUIT_SECONDS)
    except subprocess.TimeoutExpired:
        return report("Escape ends the program", False, f"still running after {QUIT_SECONDS} s")
    return report("Escape ends the program", code == 0, f"exit code {code}")


# --- Windows ---------------------------------------------------------------------------------------------------------


def windows(app):
    from ctypes import wintypes

    user32 = ctypes.windll.user32
    keys = {"Space": (0x20, 0x39), "Escape": (0x1B, 0x01)}

    def window():
        found = []

        @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
        def each(hwnd, _):
            owner = wintypes.DWORD()
            user32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
            if owner.value == app.pid and user32.IsWindowVisible(hwnd):
                title = ctypes.create_unicode_buffer(200)
                user32.GetWindowTextW(hwnd, title, 200)
                if WINDOW in title.value:
                    found.append(hwnd)
            return True

        user32.EnumWindows(each, 0)
        return found[0] if found else 0

    def press(hwnd, key):
        code, scan = keys[key]
        user32.PostMessageW(hwnd, 0x0100, code, 1 | (scan << 16))
        time.sleep(0.05)
        user32.PostMessageW(hwnd, 0x0101, code, 1 | (scan << 16) | (1 << 30) | (1 << 31))

    driver = os.environ.get("TESTING_DESKTOP_DRIVER") or shutil.which("sheet-driver")
    if not driver:
        print("check-window-keys: no sheet-driver (appkit) on the PATH and no TESTING_DESKTOP_DRIVER", file=sys.stderr)
        return 2
    hwnd = 0
    for _ in range(START_SECONDS * 10):
        hwnd = window()
        if hwnd or app.poll() is not None:
            break
        time.sleep(0.1)
    if not report("the window is there", bool(hwnd), f"exit code {app.poll()}" if not hwnd else ""):
        return 1
    reader = subprocess.Popen([driver], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)

    def ask(command):
        reader.stdin.write(json.dumps(command) + "\n")
        reader.stdin.flush()
        return json.loads(reader.stdout.readline())

    def named(name, n, timeout):
        answer = ask({"cmd": "step", "n": n, "step": {"wait": {"name": name}, "timeout": timeout}})
        return bool(answer.get("ok")), str(answer.get("reason") or "")

    opened = ask(
        {
            "cmd": "open",
            "sheet": "check-window-keys",
            "size": {"cols": 1280, "rows": 800},
            "options": {"pid": app.pid, "window": WINDOW},
        }
    )
    if not opened.get("ok"):
        print(f"check-window-keys: sheet-driver does not find the window: {opened}", file=sys.stderr)
        return 2
    good = report("it plays", *named("Pause", 1, 20000))
    press(hwnd, "Space")
    good &= report("Space pauses", *named("Play", 2, 3000))
    press(hwnd, "Space")
    good &= report("Space plays again", *named("Pause", 3, 3000))
    reader.stdin.close()
    press(hwnd, "Escape")
    good &= quits(app)
    return 0 if good else 1


# --- Linux (X11) -----------------------------------------------------------------------------------------------------


def x11(app):
    from ctypes import POINTER, byref, c_char_p, c_int, c_long, c_uint, c_ulong, c_void_p

    class Attributes(ctypes.Structure):
        _fields_ = [
            ("x", c_int), ("y", c_int), ("width", c_int), ("height", c_int), ("border_width", c_int),
            ("depth", c_int), ("visual", c_void_p), ("root", c_ulong), ("c_class", c_int),
            ("bit_gravity", c_int), ("win_gravity", c_int), ("backing_store", c_int),
            ("backing_planes", c_ulong), ("backing_pixel", c_ulong), ("save_under", c_int),
            ("colormap", c_ulong), ("map_installed", c_int), ("map_state", c_int),
            ("all_event_masks", c_long), ("your_event_mask", c_long), ("do_not_propagate_mask", c_long),
            ("override_redirect", c_int), ("screen", c_void_p),
        ]  # fmt: skip

    class Image(ctypes.Structure):
        _fields_ = [
            ("width", c_int), ("height", c_int), ("xoffset", c_int), ("format", c_int),
            ("data", POINTER(ctypes.c_char)), ("byte_order", c_int), ("bitmap_unit", c_int),
            ("bitmap_bit_order", c_int), ("bitmap_pad", c_int), ("depth", c_int), ("bytes_per_line", c_int),
        ]  # fmt: skip

    try:
        xlib = ctypes.CDLL("libX11.so.6")
        xcb = ctypes.CDLL("libxcb.so.1")
        xtest = ctypes.CDLL("libxcb-xtest.so.0")
    except OSError as error:
        print(f"check-window-keys: {error}", file=sys.stderr)
        return 2
    xlib.XOpenDisplay.restype = c_void_p
    xlib.XOpenDisplay.argtypes = [c_char_p]
    xlib.XDefaultRootWindow.restype = c_ulong
    xlib.XDefaultRootWindow.argtypes = [c_void_p]
    xlib.XQueryTree.argtypes = [
        c_void_p, c_ulong, POINTER(c_ulong), POINTER(c_ulong), POINTER(POINTER(c_ulong)), POINTER(c_uint),
    ]  # fmt: skip
    xlib.XFetchName.argtypes = [c_void_p, c_ulong, POINTER(c_char_p)]
    xlib.XGetWindowAttributes.argtypes = [c_void_p, c_ulong, POINTER(Attributes)]
    xlib.XKeysymToKeycode.argtypes = [c_void_p, c_ulong]
    xlib.XKeysymToKeycode.restype = ctypes.c_ubyte
    xlib.XSetInputFocus.argtypes = [c_void_p, c_ulong, c_int, c_ulong]
    xlib.XRaiseWindow.argtypes = [c_void_p, c_ulong]
    xlib.XSync.argtypes = [c_void_p, c_int]
    xlib.XGetImage.restype = c_void_p
    xlib.XGetImage.argtypes = [c_void_p, c_ulong, c_int, c_int, c_uint, c_uint, c_ulong, c_int]
    xlib.XFree.argtypes = [c_void_p]
    xcb.xcb_connect.restype = c_void_p
    xcb.xcb_connect.argtypes = [c_char_p, c_void_p]
    xcb.xcb_flush.argtypes = [c_void_p]
    xtest.xcb_test_fake_input.restype = c_uint
    xtest.xcb_test_fake_input.argtypes = [
        c_void_p, ctypes.c_uint8, ctypes.c_uint8, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_int16, ctypes.c_int16,
        ctypes.c_uint8,
    ]  # fmt: skip
    keys = {"Space": 0x20, "Escape": 0xFF1B}

    display = xlib.XOpenDisplay(None)
    sender = xcb.xcb_connect(None, None)
    if not display or not sender:
        print("check-window-keys: no X display (DISPLAY)", file=sys.stderr)
        return 2

    def find(window):
        name = c_char_p()
        if xlib.XFetchName(display, window, byref(name)) and name.value:
            attributes = Attributes()
            xlib.XGetWindowAttributes(display, window, byref(attributes))
            # 2: IsViewable.
            if WINDOW in name.value.decode("utf-8", "replace") and attributes.map_state == 2:
                return window
        root, parent, children, count = c_ulong(), c_ulong(), POINTER(c_ulong)(), c_uint()
        if xlib.XQueryTree(display, window, byref(root), byref(parent), byref(children), byref(count)):
            for i in range(count.value):
                hit = find(children[i])
                if hit:
                    return hit
        return 0

    def picture(window):
        attributes = Attributes()
        xlib.XGetWindowAttributes(display, window, byref(attributes))
        # All planes, format ZPixmap.
        image = xlib.XGetImage(display, window, 0, 0, attributes.width, attributes.height, 0xFFFFFFFF, 2)
        if not image:
            return None
        content = ctypes.cast(image, POINTER(Image)).contents
        return hashlib.sha256(ctypes.string_at(content.data, content.bytes_per_line * content.height)).hexdigest()

    def moves(window):
        first = picture(window)
        time.sleep(0.6)
        second = picture(window)
        if first is None or second is None:
            return None
        return first != second

    def press(window, key):
        xlib.XRaiseWindow(display, window)
        # RevertToPointerRoot, CurrentTime.
        xlib.XSetInputFocus(display, window, 1, 0)
        xlib.XSync(display, 0)
        code = xlib.XKeysymToKeycode(display, keys[key])
        xtest.xcb_test_fake_input(sender, 2, code, 0, 0, 0, 0, 0)
        xtest.xcb_test_fake_input(sender, 3, code, 0, 0, 0, 0, 0)
        xcb.xcb_flush(sender)
        time.sleep(0.5)

    window = 0
    for _ in range(START_SECONDS * 10):
        window = find(xlib.XDefaultRootWindow(display))
        if window or app.poll() is not None:
            break
        time.sleep(0.1)
    if not report("the window is there", bool(window), f"exit code {app.poll()}" if not window else ""):
        return 1
    time.sleep(2)
    playing = moves(window)
    if playing is None:
        print("check-window-keys: the picture of the window cannot be read on this display", file=sys.stderr)
        return 2
    good = report("it plays", playing, "the picture changes" if playing else "the picture stands")
    press(window, "Space")
    good &= report("Space pauses", moves(window) is False)
    press(window, "Space")
    good &= report("Space plays again", moves(window) is True)
    press(window, "Escape")
    good &= quits(app)
    return 0 if good else 1


def main():
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    program = os.path.abspath(sys.argv[1])
    if not os.path.isfile(program):
        print(f"check-window-keys: no such program: {program}", file=sys.stderr)
        return 2
    env = dict(os.environ, TAKT_AUDIO="sim")
    if sys.platform != "win32":
        # X11: the keys are XTest events, and winit takes Wayland when it finds one.
        env.pop("WAYLAND_DISPLAY", None)
    app = subprocess.Popen([program], env=env)
    try:
        return windows(app) if sys.platform == "win32" else x11(app)
    finally:
        if app.poll() is None:
            app.kill()


if __name__ == "__main__":
    sys.exit(main())

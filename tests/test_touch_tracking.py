"""Touch tracking in the daemon's Engine, driven by synthetic evdev frames:
silent slots, ghost slots, restores, slot reuse and two-finger pairing.
Needs python-evdev, like the daemon itself.

Run from the repo root:  python3 -m unittest discover -s tests -v
"""
import importlib.machinery
import importlib.util
import os
import types
import unittest
from unittest import mock

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DAEMON = os.path.join(ROOT, "bin", "magic-mouse-daemon")

try:
    loader = importlib.machinery.SourceFileLoader("magic_mouse_daemon_touch", DAEMON)
    spec = importlib.util.spec_from_loader("magic_mouse_daemon_touch", loader)
    daemon = importlib.util.module_from_spec(spec)
    loader.exec_module(daemon)
    SKIP = None
except ImportError as exc:  # python-evdev missing
    daemon = None
    SKIP = f"cannot import the daemon: {exc}"

SLOT, TID, X, Y, MAJOR = 47, 57, 53, 54, 48  # ABS_MT_* codes


class FakeUI:
    def __init__(self):
        self.wheel = {"x": 0, "y": 0}

    def write(self, typ, code, value):
        e = daemon.e
        if typ == e.EV_REL and code == e.REL_WHEEL_HI_RES:
            self.wheel["y"] += value
        elif typ == e.EV_REL and code == e.REL_HWHEEL_HI_RES:
            self.wheel["x"] += value

    def syn(self):
        pass


class EngineCase(unittest.TestCase):
    def setUp(self):
        daemon.log = lambda msg: None
        self.actions = []
        daemon.run_action = lambda cmd: self.actions.append(cmd)
        self.cfg = {k: dict(v) for k, v in daemon.DEFAULTS.items()}
        self.ui = FakeUI()
        self.eng = daemon.Engine(self.cfg, self.ui, False)
        self.t = 100.0

    def frame(self, *events):
        """One input report: (code, value) ABS_MT events, then SYN_REPORT."""
        for code, value in events:
            self.eng.feed(types.SimpleNamespace(type=3, code=code, value=value, timestamp=lambda t=self.t: t))
        self.eng.feed(types.SimpleNamespace(type=0, code=0, value=0, timestamp=lambda t=self.t: t))
        if self.eng.coasting:
            self.eng.tick(self.t)
        self.t += 0.011

    def idle(self, n):
        for _ in range(n):
            self.frame()


@unittest.skipIf(SKIP, SKIP)
class TouchTracking(EngineCase):
    def test_ghost_slot_from_sync_expires(self):
        # a slot loaded by sync_mt that the kernel never mentions again
        self.eng.slots = {5: [900, 0, 600, 400, 300]}
        self.idle(int(daemon.STALE_SEC / 0.011) + 5)
        self.assertNotIn(5, self.eng.slots)

    def test_silent_resting_finger_is_kept_then_restored_as_same_touch(self):
        self.frame((SLOT, 1), (TID, 7), (X, 400), (Y, 200), (MAJOR, 600))
        self.idle(5)  # silent, but within STALE_SEC
        self.assertIn(7, self.eng.touches)
        self.idle(int(daemon.STALE_SEC / 0.011) + 5)  # now parked
        self.assertNotIn(7, self.eng.touches)
        before = self.eng.parked_touch[7]
        self.frame((SLOT, 1), (X, 404))  # speaks again, no new tracking id
        self.assertIs(self.eng.touches.get(7), before)

    def test_restore_does_not_stop_a_glide(self):
        self.frame((SLOT, 1), (TID, 7), (X, 400), (Y, 1500), (MAJOR, 600))  # resting finger
        self.frame((SLOT, 0), (TID, 8), (X, 0), (Y, 1200), (MAJOR, 600))
        for i in range(25):  # fast scroll with the other finger
            self.frame((SLOT, 0), (Y, 1200 - 60 * i))
        self.frame((SLOT, 0), (TID, -1))
        self.idle(int(daemon.BRIDGE_MS / 0.011) + 2)  # the glide starts once the lift is confirmed
        self.assertTrue(self.eng.coasting)
        self.idle(int(daemon.STALE_SEC / 0.011) - 6)  # resting finger gets parked (still gliding)
        self.assertNotIn(7, self.eng.touches)
        self.assertTrue(self.eng.coasting)
        self.frame((SLOT, 1), (X, 402))  # and twitches
        self.assertTrue(self.eng.coasting)

    def test_lift_is_never_restored(self):
        self.frame((SLOT, 1), (TID, 7), (X, 400), (Y, 200))
        self.idle(int(daemon.STALE_SEC / 0.011) + 5)
        self.frame((SLOT, 1), (TID, -1))
        self.frame((SLOT, 1), (X, 410))  # stray position without an id
        self.assertNotIn(7, self.eng.touches)

    def test_jump_without_new_id_is_a_new_landing(self):
        self.frame((SLOT, 0), (TID, 9), (X, 0), (Y, 0))
        first = self.eng.touches[9]
        self.frame((SLOT, 0), (X, int(daemon.JUMP_MM * 1.5 * daemon.TOUCH_UNITS_PER_MM)))
        self.assertIsNot(self.eng.touches[9], first)

    def test_two_fingers_moving_together_are_followed_as_a_pair(self):
        self.frame((SLOT, 0), (TID, 1), (X, -300), (Y, 400), (MAJOR, 600),
                   (SLOT, 1), (TID, 2), (X, 300), (Y, 450), (MAJOR, 600))
        for i in range(1, 30):  # both slide backwards, the second a bit slower
            self.frame((SLOT, 0), (Y, 400 + 30 * i), (SLOT, 1), (Y, 450 + 18 * i))
        self.assertEqual(self.eng.primary, (1, 2))
        self.assertTrue(self.eng.scrolling)

    def test_partner_that_stops_is_dropped(self):
        self.frame((SLOT, 0), (TID, 1), (X, -300), (Y, 400), (MAJOR, 600),
                   (SLOT, 1), (TID, 2), (X, 300), (Y, 450), (MAJOR, 600))
        for i in range(1, 20):
            self.frame((SLOT, 0), (Y, 400 + 30 * i), (SLOT, 1), (Y, 450 + 25 * i))
        for i in range(20, 45):  # finger 2 rests, finger 1 keeps going
            self.frame((SLOT, 0), (Y, 400 + 30 * i))
        self.assertEqual(self.eng.primary, (1,))



@unittest.skipIf(SKIP, SKIP)
class TouchConfirm(EngineCase):
    """Hysteresis on contact size: hover does not scroll, a firm touch does and keeps going."""

    def slide(self, slot, n, size, gap=0.011, y0=0, step=40):
        for i in range(n):
            self.frame((SLOT, slot), (Y, y0 + step * i), (MAJOR, size))
            self.t += gap - 0.011

    def test_hover_with_big_but_sparse_reports_does_not_scroll(self):
        self.frame((SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600))
        self.slide(0, 30, 600, gap=0.03)
        self.assertFalse(self.eng.touches[1].ok)
        self.assertEqual(self.ui.wheel["y"], 0)

    def test_small_contact_does_not_scroll(self):
        self.frame((SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 200))
        self.slide(0, 30, 200)
        self.assertEqual(self.ui.wheel["y"], 0)

    def test_firm_touch_scrolls_and_keeps_scrolling_when_lighter(self):
        self.frame((SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600))
        self.slide(0, 3, 600)
        self.assertTrue(self.eng.touches[1].ok)
        self.slide(0, 25, 320, y0=120)  # less of the fingertip touches while it slides
        self.assertTrue(self.eng.scrolling)
        self.assertNotEqual(self.ui.wheel["y"], 0)

    def test_dropping_below_hold_stops_counting(self):
        self.frame((SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600))
        self.slide(0, 3, 600)
        self.slide(0, 2, 200, y0=120)
        self.assertFalse(self.eng.touches[1].ok)

    def test_second_finger_needs_only_the_hold_size(self):
        self.frame((SLOT, 0), (TID, 1), (X, -300), (Y, 0), (MAJOR, 600))
        self.slide(0, 3, 600)
        self.frame((SLOT, 1), (TID, 2), (X, 300), (Y, 0), (MAJOR, 350))
        self.assertTrue(self.eng.touches[2].ok)

    def test_two_light_fingers_count_together(self):
        self.frame((SLOT, 0), (TID, 1), (X, -300), (Y, 0), (MAJOR, 280),
                   (SLOT, 1), (TID, 2), (X, 300), (Y, 0), (MAJOR, 280))
        self.assertTrue(self.eng.touches[1].ok and self.eng.touches[2].ok)

    def test_one_light_finger_alone_does_not_count(self):
        self.frame((SLOT, 0), (TID, 1), (X, -300), (Y, 0), (MAJOR, 280))
        self.slide(0, 10, 280)
        self.assertFalse(self.eng.touches[1].ok)

    def test_off_counts_every_touch(self):
        self.cfg["scroll"]["touch_start_size"] = 0
        self.frame((SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 100))
        self.assertTrue(self.eng.touches[1].ok)


class FakePad:
    def __init__(self):
        self.calls = []
        self.pos = []

    def update(self, fingers, gain):
        self.calls.append(([f[0] for f in fingers], gain))
        self.pos.append([(f[1], f[2]) for f in fingers])


@unittest.skipIf(SKIP, SKIP)
class TouchpadMode(EngineCase):
    """Touchpad mode hands the followed fingers to the virtual touchpad and scrolls nothing itself."""

    def setUp(self):
        super().setUp()
        self.pad = self.eng.vpad = FakePad()

    def test_one_finger_goes_to_the_touchpad_without_wheel_events(self):
        self.frame((SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600))
        for i in range(1, 20):
            self.frame((SLOT, 0), (Y, 40 * i), (MAJOR, 600))
        self.assertEqual(self.pad.calls[-1][0], [1])
        self.assertEqual(self.ui.wheel, {"x": 0, "y": 0})

    def test_two_fingers_moving_together_go_as_a_pair(self):
        self.frame((SLOT, 0), (TID, 1), (X, -300), (Y, 400), (MAJOR, 600),
                   (SLOT, 1), (TID, 2), (X, 300), (Y, 450), (MAJOR, 600))
        for i in range(1, 20):
            self.frame((SLOT, 0), (X, -300 + 30 * i), (SLOT, 1), (X, 300 + 28 * i))
        self.assertEqual(self.pad.calls[-1][0], [1, 2])
        self.assertFalse(self.actions)  # no workspace action of our own

    def test_lift_lifts_the_touchpad(self):
        self.frame((SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600))
        self.frame((SLOT, 0), (Y, 40))
        self.frame((SLOT, 0), (TID, -1))
        self.assertEqual(self.pad.calls[-1][0], [])


class RecordingUI:
    """Collects what a VPad writes: per virtual tracking id, its x positions."""

    def __init__(self):
        self.slot, self.cur, self.xs = 0, {}, {}

    def write(self, typ, code, value):
        e = daemon.e
        if typ != e.EV_ABS:
            return
        if code == e.ABS_MT_SLOT:
            self.slot = value
        elif code == e.ABS_MT_TRACKING_ID:
            self.cur[self.slot] = value
        elif code == e.ABS_MT_POSITION_X:
            self.xs.setdefault(self.cur[self.slot], []).append(value)

    def syn(self):
        pass


@unittest.skipIf(SKIP, SKIP)
class VirtualTouchpad(unittest.TestCase):
    def pad(self):
        p = daemon.VPad.__new__(daemon.VPad)
        p.xr, p.yr, p.res = (-6000, 6000), (-9000, 9000), 38.0
        p.ui = RecordingUI()
        p.slots, p.ids, p.next_id, p.extra, p.extra_pos, p.count = {}, {}, 0, None, None, 0
        p.gain, p.anchor, p.last = 1.0, {}, {}
        return p

    def test_no_virtual_finger_jumps_when_the_pair_becomes_one(self):
        p = self.pad()
        for i in range(10):  # two fingers swipe, then the first lifts, the second goes on
            p.update([(1, -300 + 20 * i, 0), (2, 300 + 20 * i, 0)], 2.7)
        for i in range(10, 20):
            p.update([(2, 300 + 20 * i, 0)], 2.7)
        for xs in p.ui.xs.values():
            steps = [abs(b - a) for a, b in zip(xs, xs[1:])]
            self.assertLessEqual(max(steps or [0]), 2.7 * 20 + 1)

    def test_new_finger_lands_next_to_the_one_already_down(self):
        p = self.pad()
        for i in range(10):
            p.update([(1, 20 * i, 0), (2, 400 + 20 * i, 0)], 3.0)  # gain moves the virtual fingers away
        p.update([(2, 600, 0)], 3.0)
        p.update([(2, 600, 0), (3, 900, 0)], 3.0)
        v2, v3 = p.last[2], p.last[3]
        self.assertAlmostEqual(v3[0] - v2[0], 300, delta=1)  # same distance as on the mouse


@unittest.skipIf(SKIP, SKIP)
class TouchpadSteadiness(EngineCase):
    def setUp(self):
        super().setUp()
        self.pad = self.eng.vpad = FakePad()

    def test_pair_does_not_flap_while_one_finger_slows(self):
        self.frame((SLOT, 0), (TID, 1), (X, -300), (Y, 400), (MAJOR, 600),
                   (SLOT, 1), (TID, 2), (X, 300), (Y, 450), (MAJOR, 600))
        for i in range(1, 15):
            self.frame((SLOT, 0), (X, -300 + 40 * i), (SLOT, 1), (X, 300 + 40 * i))
        for i in range(15, 30):  # the second finger nearly stops, the first goes on
            self.frame((SLOT, 0), (X, -300 + 40 * i), (SLOT, 1), (X, 300 + 40 * 15 + i))
        counts = [len(c[0]) for c in self.pad.calls if c[0]]
        changes = sum(1 for a, b in zip(counts, counts[1:]) if a != b)
        self.assertLessEqual(changes, 1)

    def test_second_finger_during_a_scroll_is_ignored(self):
        self.frame((SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600))
        for i in range(1, 15):
            self.frame((SLOT, 0), (Y, 40 * i), (MAJOR, 600))
        for i in range(15, 40):  # a second finger lands beside it and slides along
            ev = [(SLOT, 0), (Y, 40 * i), (MAJOR, 600)]
            if i == 15:
                ev += [(SLOT, 1), (TID, 2), (X, 500), (Y, 600), (MAJOR, 600)]
            else:
                ev += [(SLOT, 1), (Y, 600 + 30 * (i - 15))]
            self.frame(*ev)
        self.assertTrue(all(c[0] == [1] for c in self.pad.calls if c[0]))

    def test_two_fingers_up_and_down_scroll_as_one(self):
        self.frame((SLOT, 0), (TID, 1), (X, -300), (Y, 0), (MAJOR, 600),
                   (SLOT, 1), (TID, 2), (X, 300), (Y, 50), (MAJOR, 600))
        for i in range(1, 20):
            self.frame((SLOT, 0), (Y, 40 * i), (SLOT, 1), (Y, 50 + 38 * i))
        down = [c[0] for c in self.pad.calls if c[0]]
        self.assertTrue(down and all(len(f) == 1 for f in down))

    def test_finger_reporting_light_for_a_moment_stays_down(self):
        self.frame((SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600))
        for i in range(1, 10):
            self.frame((SLOT, 0), (Y, 40 * i), (MAJOR, 600))
        for i in range(10, 14):  # four reports below the hold size while it slides on
            self.frame((SLOT, 0), (Y, 40 * i), (MAJOR, 150))
        for i in range(14, 20):
            self.frame((SLOT, 0), (Y, 40 * i), (MAJOR, 600))
        first = next(i for i, c in enumerate(self.pad.calls) if c[0])
        self.assertTrue(all(c[0] == [1] for c in self.pad.calls[first:]))

    def test_finger_left_after_a_swipe_does_not_scroll(self):
        self.frame((SLOT, 0), (TID, 1), (X, -300), (Y, 400), (MAJOR, 600),
                   (SLOT, 1), (TID, 2), (X, 300), (Y, 450), (MAJOR, 600))
        for i in range(1, 15):
            self.frame((SLOT, 0), (X, -300 + 40 * i), (SLOT, 1), (X, 300 + 40 * i))
        self.assertEqual(self.pad.calls[-1][0], [1, 2])
        self.frame((SLOT, 0), (TID, -1))
        for i in range(15, 30):  # the other finger slides on
            self.frame((SLOT, 1), (Y, 450 + 40 * (i - 15)))
        self.assertEqual(self.pad.calls[-1][0], [])

    def test_moving_the_mouse_while_scrolling_keeps_the_finger_down(self):
        self.frame((SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600))
        for i in range(1, 10):
            self.frame((SLOT, 0), (Y, 40 * i), (MAJOR, 600))
        for i in range(10, 25):  # the hand moves the mouse too, farther than the finger slides
            self.eng.feed(types.SimpleNamespace(type=2, code=0, value=30, timestamp=lambda t=self.t: t))
            self.frame((SLOT, 0), (Y, 400 + 5 * (i - 10)), (MAJOR, 600))
        first = next(i for i, c in enumerate(self.pad.calls) if c[0])
        self.assertTrue(all(c[0] == [1] for c in self.pad.calls[first:]))

    def test_short_flick_lands_where_it_began(self):
        self.frame((SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600))
        for i in range(1, 16):
            self.frame((SLOT, 0), (Y, 40 * i), (MAJOR, 600))
        first = next(i for i, c in enumerate(self.pad.calls) if c[0])
        y_down = self.pad.pos[first][0][1]
        self.assertLess(y_down, 40 * (first + 1) - 40)  # behind the finger: the slide so far is replayed
        self.assertAlmostEqual(self.pad.pos[-1][0][1], self.eng.touches[1].y, delta=2)  # caught up

    def test_finger_staying_light_lifts(self):
        self.frame((SLOT, 0), (TID, 1), (X, 0), (Y, 0), (MAJOR, 600))
        for i in range(1, 10):
            self.frame((SLOT, 0), (Y, 40 * i), (MAJOR, 600))
        for i in range(10, 25):  # hovering on: no longer a touch
            self.frame((SLOT, 0), (Y, 400), (MAJOR, 150))
        self.assertEqual(self.pad.calls[-1][0], [])


@unittest.skipIf(SKIP, SKIP)
class WorkspaceSwipeEnd(unittest.TestCase):
    """workspace_step against a fake Hyprland: 1, 2 and 4 open on monitor 0, 3 on monitor 1."""

    def step(self, current, forward, end, monitor=0, empty=False):
        sent, ran = [], []
        replies = {"j/workspaces": '[{"id":1,"monitorID":0},{"id":2,"monitorID":0},{"id":3,"monitorID":1},'
                                   '{"id":4,"monitorID":0},{"id":-98,"monitorID":0}]',
                   "j/activeworkspace": '{"id":%d,"monitorID":%d}' % (current, monitor)}

        def request(cmd, timeout=0.5):
            if cmd.startswith("j/"):
                return replies.get(cmd)
            sent.append(cmd.split('"')[1])

        class Now:  # run the worker inline
            def __init__(self, target, daemon=None):
                self.target = target

            def start(self):
                self.target()
        with mock.patch.object(daemon, "hypr_request", request), \
                mock.patch.object(daemon, "run_action", ran.append), \
                mock.patch.object(daemon, "threading", types.SimpleNamespace(Thread=Now, Lock=daemon.threading.Lock)):
            daemon.workspace_step(forward, end, "FALLBACK", empty)
        return sent, ran

    def test_next_open_workspace_on_this_monitor(self):
        self.assertEqual(self.step(2, True, "stop"), (["4"], []))  # 3 is on the other monitor

    def test_end_behaviour_forward(self):
        self.assertEqual(self.step(4, True, "stop"), ([], []))
        self.assertEqual(self.step(4, True, "new"), (["5"], []))
        self.assertEqual(self.step(4, True, "wrap"), (["1"], []))

    def test_end_behaviour_backward(self):
        self.assertEqual(self.step(1, False, "stop"), ([], []))
        self.assertEqual(self.step(1, False, "new"), ([], []))
        self.assertEqual(self.step(1, False, "wrap"), (["4"], []))

    def test_include_empty_reaches_empty_workspaces(self):
        self.assertEqual(self.step(4, True, "stop", empty=True), (["5"], []))  # 1-5 always count
        self.assertEqual(self.step(2, True, "stop", empty=True), (["4"], []))  # 3 is on the other monitor
        self.assertEqual(self.step(5, True, "stop", empty=True), ([], []))
        self.assertEqual(self.step(5, True, "new", empty=True), (["6"], []))
        self.assertEqual(self.step(5, True, "wrap", empty=True), (["1"], []))
        self.assertEqual(self.step(1, False, "wrap", empty=True), (["5"], []))

    def test_new_stays_within_ten(self):
        self.assertEqual(self.step(10, True, "new", empty=True), ([], []))

    def test_named_workspace_runs_the_plain_action(self):
        self.assertEqual(self.step(-5, True, "stop"), ([], ["FALLBACK"]))


if __name__ == "__main__":
    unittest.main()

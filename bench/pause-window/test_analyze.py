"""
Unit tests for analyze.py — runs on synthetic event lists so we
don't need a real firecracker / KVM to exercise the analysis path.

CI runs this via `python -m unittest`. Keeping zero third-party
deps so it works in the same environment as the agent itself.
"""
import unittest

from analyze import analyze, pct


def synth_clean_run(n_pings: int = 100, interval_ms: int = 100, rtt_ms: int = 2):
    """A trial with no pause: pings every interval, all answered."""
    events = [{"event": "start", "interval_ms": interval_ms}]
    for seq in range(1, n_pings + 1):
        t_send = seq * interval_ms
        t_recv = t_send + rtt_ms
        events.append({"event": "send", "seq": seq, "t_send_ms": t_send})
        events.append(
            {
                "event": "recv",
                "seq": seq,
                "t_send_ms": t_send,
                "t_recv_ms": t_recv,
                "rtt_ms": rtt_ms,
            }
        )
    events.append(
        {
            "event": "stop",
            "sent": n_pings,
            "recv": n_pings,
            "timeouts": 0,
            "errors": 0,
        }
    )
    return events


def synth_paused_run(
    pre_pings: int = 30,
    pause_ms: int = 2000,
    post_pings: int = 30,
    in_flight_during_pause: int = 5,
    interval_ms: int = 100,
    rtt_ms: int = 2,
):
    """Pre-pause traffic, then a gap that swallows `in_flight_during_pause`
    sends, then resumed traffic with answers."""
    events = [{"event": "start", "interval_ms": interval_ms}]
    t = 0
    seq = 0

    # Pre-pause: send + recv pairs.
    for _ in range(pre_pings):
        seq += 1
        t += interval_ms
        events.append({"event": "send", "seq": seq, "t_send_ms": t})
        events.append(
            {
                "event": "recv",
                "seq": seq,
                "t_send_ms": t,
                "t_recv_ms": t + rtt_ms,
                "rtt_ms": rtt_ms,
            }
        )

    last_pre_recv_ms = t + rtt_ms

    # During the pause: sends with no recvs. These are the in-flight
    # losses the agent reports as timeouts (we model them as `send`
    # with no matching `recv`).
    for _ in range(in_flight_during_pause):
        seq += 1
        t += interval_ms
        events.append({"event": "send", "seq": seq, "t_send_ms": t})

    # Resumed traffic. Jump time forward so the first post-recv
    # creates the gap we want.
    first_post_recv_ms = last_pre_recv_ms + pause_ms
    t = first_post_recv_ms
    for i in range(post_pings):
        seq += 1
        t_send = first_post_recv_ms + i * interval_ms
        events.append({"event": "send", "seq": seq, "t_send_ms": t_send})
        events.append(
            {
                "event": "recv",
                "seq": seq,
                "t_send_ms": t_send,
                "t_recv_ms": t_send + rtt_ms,
                "rtt_ms": rtt_ms,
            }
        )

    events.append(
        {
            "event": "stop",
            "sent": seq,
            "recv": pre_pings + post_pings,
            "timeouts": in_flight_during_pause,
            "errors": 0,
        }
    )
    return events


class PercentileTests(unittest.TestCase):
    def test_empty_returns_zero(self):
        self.assertEqual(pct([], 50), 0.0)

    def test_p50_on_known_distribution(self):
        # [1..9], p50 = 5.0
        self.assertEqual(pct(list(range(1, 10)), 50), 5.0)

    def test_p99_top_bound(self):
        # p99 of [1..100] is 99.01 with linear interp
        self.assertAlmostEqual(pct(list(range(1, 101)), 99), 99.01, places=2)

    def test_p0_and_p100_clamp(self):
        self.assertEqual(pct([1, 2, 3], 0), 1.0)
        self.assertEqual(pct([1, 2, 3], 100), 3.0)


class AnalyzeCleanRunTests(unittest.TestCase):
    def setUp(self):
        self.events = synth_clean_run(n_pings=100)

    def test_no_pause_detected(self):
        r = analyze(self.events)
        self.assertFalse(r["pause"]["detected"])

    def test_counts_match(self):
        r = analyze(self.events)
        self.assertEqual(r["sent"], 100)
        self.assertEqual(r["recv"], 100)
        self.assertEqual(r["timeouts"], 0)
        self.assertEqual(r["errors"], 0)
        self.assertEqual(r["pause"]["in_flight_lost"], 0)

    def test_connection_survived(self):
        r = analyze(self.events)
        self.assertTrue(r["connection_survived"])

    def test_rtt_consistent(self):
        r = analyze(self.events)
        self.assertEqual(r["rtt_ms"]["all_p50"], 2)
        # No after-pause segment when no pause; key still present.
        self.assertEqual(r["rtt_ms"]["after_p50"], 0.0)


class AnalyzePausedRunTests(unittest.TestCase):
    def setUp(self):
        self.events = synth_paused_run(
            pre_pings=30,
            pause_ms=2000,
            post_pings=30,
            in_flight_during_pause=5,
        )

    def test_pause_detected(self):
        r = analyze(self.events)
        self.assertTrue(r["pause"]["detected"])

    def test_pause_duration_in_range(self):
        r = analyze(self.events)
        # We modelled 2000 ms gap minus 100 ms baseline = 1900 ms,
        # plus the recv-to-recv jitter we built in.
        self.assertGreaterEqual(r["pause"]["app_duration_ms"], 1800)
        self.assertLessEqual(r["pause"]["app_duration_ms"], 2100)

    def test_in_flight_loss_counted(self):
        r = analyze(self.events)
        self.assertEqual(r["pause"]["in_flight_lost"], 5)

    def test_connection_survived_with_post_recvs(self):
        r = analyze(self.events)
        self.assertTrue(r["connection_survived"])

    def test_daemon_pause_threaded_through(self):
        r = analyze(self.events, daemon_pause_ms=1750)
        self.assertEqual(r["daemon_pause_ms"], 1750)
        # And the report distinguishes app-observed from daemon-measured.
        self.assertNotEqual(r["pause"]["app_duration_ms"], r["daemon_pause_ms"])


class AnalyzeBaselineSensitivityTests(unittest.TestCase):
    def test_short_blip_at_lower_baseline_now_detects(self):
        # A 200 ms gap is normal at baseline=100 (≤3x), but anomalous
        # at baseline=30 (>3x). Same data, different verdict — the
        # baseline param has to actually take effect.
        events = [
            {"event": "start"},
            {"event": "send", "seq": 1, "t_send_ms": 0},
            {"event": "recv", "seq": 1, "t_send_ms": 0, "t_recv_ms": 5, "rtt_ms": 5},
            {"event": "send", "seq": 2, "t_send_ms": 205},
            {"event": "recv", "seq": 2, "t_send_ms": 205, "t_recv_ms": 210, "rtt_ms": 5},
            {"event": "stop", "sent": 2, "recv": 2, "timeouts": 0, "errors": 0},
        ]
        r_loose = analyze(events, baseline_interval_ms=100)
        self.assertFalse(r_loose["pause"]["detected"])
        r_tight = analyze(events, baseline_interval_ms=30)
        self.assertTrue(r_tight["pause"]["detected"])


if __name__ == "__main__":
    unittest.main()
